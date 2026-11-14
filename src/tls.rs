//! TLS and mutual TLS (mTLS) server configuration, client-cert inspection, and HTTPS listener adapter.

use crate::history::unix_timestamp;
use crate::logging::{EventForwarder, LogEvent, Severity};
use crate::metrics::MetricsRegistry;
use anyhow::{anyhow, bail, Context, Result};
use axum::serve::Listener;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::server::WebPkiClientVerifier;
use tokio_rustls::rustls::{RootCertStore, ServerConfig, ServerConnection};
use tokio_rustls::server::TlsStream;
use tokio_rustls::TlsAcceptor;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
const ACCEPT_QUEUE_CAPACITY: usize = 256;

pub const DEFAULT_TLS_RELOAD_ENABLED: bool = true;
pub const DEFAULT_TLS_WATCH_INTERVAL_MS: u64 = 2000;
pub const DEFAULT_TLS_RELOAD_DEBOUNCE_MS: u64 = 250;
pub const MAX_TLS_WATCH_INTERVAL_MS: u64 = 86_400_000;
pub const MAX_TLS_RELOAD_DEBOUNCE_MS: u64 = 60_000;

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

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(&mut hex, "{byte:02x}");
    }
    hex
}

fn sha256_bytes(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn read_and_hash(path: &Path, label: &str) -> Result<(Vec<u8>, [u8; 32])> {
    let data =
        std::fs::read(path).with_context(|| format!("read {label}: {}", path.display()))?;
    let digest = sha256_bytes(&data);
    Ok((data, digest))
}

fn parse_certs(data: &[u8], path: &Path, label: &str) -> Result<Vec<CertificateDer<'static>>> {
    let mut slice = data;
    let certs: Vec<_> = rustls_pemfile::certs(&mut slice)
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("parse {label} certificates from {}", path.display()))?;
    if certs.is_empty() {
        bail!("no certificates found in {}", path.display());
    }
    Ok(certs)
}

fn parse_private_key(data: &[u8], path: &Path) -> Result<PrivateKeyDer<'static>> {
    let mut slice = data;
    rustls_pemfile::private_key(&mut slice)
        .with_context(|| format!("parse TLS private key from {}", path.display()))?
        .ok_or_else(|| anyhow!("no private key found in {}", path.display()))
}

#[cfg(test)]
fn load_certs(path: &Path, label: &str) -> Result<Vec<CertificateDer<'static>>> {
    let (data, _) = read_and_hash(path, label)?;
    parse_certs(&data, path, label)
}

#[cfg(test)]
fn load_private_key(path: &Path) -> Result<PrivateKeyDer<'static>> {
    let (data, _) = read_and_hash(path, "TLS key")?;
    parse_private_key(&data, path)
}

/// Ensure a process-wide rustls CryptoProvider is installed before constructing
/// any `ServerConfig` or `ClientConfig`.
pub(crate) fn ensure_crypto_provider() {
    if tokio_rustls::rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = tokio_rustls::rustls::crypto::aws_lc_rs::default_provider().install_default();
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WatchedFileDigests {
    cert: [u8; 32],
    key: [u8; 32],
    client_ca: Option<[u8; 32]>,
}

fn read_watched_file_digests(spec: &TlsServerConfigSpec) -> Result<WatchedFileDigests> {
    let (_, cert) = read_and_hash(&spec.cert_path, "TLS cert")?;
    let (_, key) = read_and_hash(&spec.key_path, "TLS key")?;
    let client_ca = match spec.client_auth {
        ClientAuthMode::None => None,
        ClientAuthMode::Optional | ClientAuthMode::Required => {
            let ca_path = spec.client_ca_path.as_deref().ok_or_else(|| {
                anyhow!("tls_client_ca must be configured when tls_client_auth is enabled")
            })?;
            let (_, ca_digest) = read_and_hash(ca_path, "TLS client CA")?;
            Some(ca_digest)
        }
    };
    Ok(WatchedFileDigests {
        cert,
        key,
        client_ca,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TlsSnapshotMetadata {
    server_cert_sha256: String,
    server_cert_chain_len: usize,
    client_ca_sha256: Vec<String>,
    client_ca_count: usize,
}

fn build_tls_acceptor_with_snapshot(
    spec: &TlsServerConfigSpec,
) -> Result<(TlsAcceptor, TlsSnapshotMetadata, WatchedFileDigests)> {
    ensure_crypto_provider();
    let (cert_bytes, cert_file_digest) = read_and_hash(&spec.cert_path, "TLS cert")?;
    let certs = parse_certs(&cert_bytes, &spec.cert_path, "TLS cert")?;
    let server_cert_sha256 = sha256_hex(certs[0].as_ref());
    let server_cert_chain_len = certs.len();

    let (key_bytes, key_file_digest) = read_and_hash(&spec.key_path, "TLS key")?;
    let key = parse_private_key(&key_bytes, &spec.key_path)?;

    let builder = ServerConfig::builder();
    let (config, client_ca_file_digest, client_ca_sha256) = match spec.client_auth {
        ClientAuthMode::None => (
            builder
                .with_no_client_auth()
                .with_single_cert(certs, key)
                .context("build TLS server config")?,
            None,
            Vec::new(),
        ),
        ClientAuthMode::Optional | ClientAuthMode::Required => {
            let ca_path = spec.client_ca_path.as_deref().ok_or_else(|| {
                anyhow!("tls_client_ca must be configured when tls_client_auth is enabled")
            })?;
            let (ca_bytes, ca_file_digest) = read_and_hash(ca_path, "TLS client CA")?;
            let ca_certs = parse_certs(&ca_bytes, ca_path, "TLS client CA")?;
            let mut ca_fingerprints = Vec::with_capacity(ca_certs.len());
            let mut roots = RootCertStore::empty();
            for cert in ca_certs {
                ca_fingerprints.push(sha256_hex(cert.as_ref()));
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
            let cfg = builder
                .with_client_cert_verifier(verifier)
                .with_single_cert(certs, key)
                .context("build mTLS server config")?;
            (cfg, Some(ca_file_digest), ca_fingerprints)
        }
    };

    let metadata = TlsSnapshotMetadata {
        server_cert_sha256,
        server_cert_chain_len,
        client_ca_count: client_ca_sha256.len(),
        client_ca_sha256,
    };
    let digests = WatchedFileDigests {
        cert: cert_file_digest,
        key: key_file_digest,
        client_ca: client_ca_file_digest,
    };
    Ok((TlsAcceptor::from(Arc::new(config)), metadata, digests))
}

/// Build a `tokio_rustls::TlsAcceptor` configured for server TLS and optional mTLS.
#[allow(dead_code)]
pub fn build_tls_acceptor(spec: &TlsServerConfigSpec) -> Result<TlsAcceptor> {
    build_tls_acceptor_with_snapshot(spec).map(|(acceptor, _, _)| acceptor)
}

/// Thread-safe, hot-swappable handle to the active `TlsAcceptor`.
///
/// New incoming connections clone the current `TlsAcceptor` (`Arc<ServerConfig>`)
/// while existing connections continue uninterrupted on their negotiated session.
#[derive(Clone)]
pub struct DynamicTlsAcceptor {
    inner: Arc<RwLock<TlsAcceptor>>,
}

impl DynamicTlsAcceptor {
    pub fn new(acceptor: TlsAcceptor) -> Self {
        Self {
            inner: Arc::new(RwLock::new(acceptor)),
        }
    }

    pub fn current(&self) -> TlsAcceptor {
        self.inner
            .read()
            .expect("dynamic TLS acceptor lock poisoned")
            .clone()
    }

    fn swap(&self, new_acceptor: TlsAcceptor) {
        *self
            .inner
            .write()
            .expect("dynamic TLS acceptor lock poisoned") = new_acceptor;
    }
}

impl From<TlsAcceptor> for DynamicTlsAcceptor {
    fn from(acceptor: TlsAcceptor) -> Self {
        Self::new(acceptor)
    }
}

/// Configuration for automatic TLS certificate & CA bundle hot-reloading.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct TlsReloadConfig {
    /// Enable automatic background reload (file watcher and Unix `SIGHUP`).
    pub enabled: bool,
    /// File-watcher polling interval in milliseconds (`0` disables file polling).
    pub watch_interval_ms: u64,
    /// Quiet period in milliseconds after detecting a file change before reloading.
    pub debounce_ms: u64,
}

impl Default for TlsReloadConfig {
    fn default() -> Self {
        Self {
            enabled: DEFAULT_TLS_RELOAD_ENABLED,
            watch_interval_ms: DEFAULT_TLS_WATCH_INTERVAL_MS,
            debounce_ms: DEFAULT_TLS_RELOAD_DEBOUNCE_MS,
        }
    }
}

impl TlsReloadConfig {
    pub fn validate(&self) -> Result<()> {
        if self.watch_interval_ms > MAX_TLS_WATCH_INTERVAL_MS {
            bail!(
                "tls_watch_interval_ms must be at most {MAX_TLS_WATCH_INTERVAL_MS} ms (got {})",
                self.watch_interval_ms
            );
        }
        if self.debounce_ms > MAX_TLS_RELOAD_DEBOUNCE_MS {
            bail!(
                "tls_reload_debounce_ms must be at most {MAX_TLS_RELOAD_DEBOUNCE_MS} ms (got {})",
                self.debounce_ms
            );
        }
        Ok(())
    }
}

/// Metadata describing the currently active server TLS certificate and mTLS CA bundle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TlsSnapshotInfo {
    pub cert_path: String,
    pub key_path: String,
    pub client_ca_path: Option<String>,
    pub client_auth: String,
    pub server_cert_sha256: String,
    pub server_cert_chain_len: usize,
    pub client_ca_sha256: Vec<String>,
    pub client_ca_count: usize,
    pub generation: u64,
    pub loaded_at: i64,
}

/// Diagnostic and operational status for dynamic TLS hot-reloading.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TlsReloadStatus {
    pub enabled: bool,
    pub watch_interval_ms: u64,
    pub debounce_ms: u64,
    pub reload_count: u64,
    pub failure_count: u64,
    pub last_reload_at: Option<i64>,
    pub last_trigger: Option<String>,
    pub last_attempt_at: Option<i64>,
    pub last_error: Option<String>,
    pub active: TlsSnapshotInfo,
}

struct TlsReloaderState {
    reload_count: u64,
    failure_count: u64,
    last_reload_at: Option<i64>,
    last_trigger: Option<String>,
    last_attempt_at: Option<i64>,
    last_error: Option<String>,
    active: TlsSnapshotInfo,
    last_loaded_digests: WatchedFileDigests,
    last_attempted_digests: Option<WatchedFileDigests>,
}

/// Coordinates atomic TLS certificate and mTLS CA bundle hot-reloading across
/// IRC TLS and HTTPS listeners without interrupting active connections.
pub struct TlsReloader {
    spec: TlsServerConfigSpec,
    config: TlsReloadConfig,
    acceptor: DynamicTlsAcceptor,
    logger: EventForwarder,
    metrics: Arc<MetricsRegistry>,
    reload_lock: Mutex<()>,
    state: Mutex<TlsReloaderState>,
}

impl TlsReloader {
    pub fn new(
        spec: TlsServerConfigSpec,
        config: TlsReloadConfig,
        logger: EventForwarder,
        metrics: Arc<MetricsRegistry>,
    ) -> Result<Self> {
        config.validate()?;
        let (initial_acceptor, metadata, digests) = build_tls_acceptor_with_snapshot(&spec)?;
        let now = unix_timestamp();
        let active = TlsSnapshotInfo {
            cert_path: spec.cert_path.display().to_string(),
            key_path: spec.key_path.display().to_string(),
            client_ca_path: spec
                .client_ca_path
                .as_ref()
                .map(|p| p.display().to_string()),
            client_auth: spec.client_auth.as_str().to_string(),
            server_cert_sha256: metadata.server_cert_sha256,
            server_cert_chain_len: metadata.server_cert_chain_len,
            client_ca_sha256: metadata.client_ca_sha256,
            client_ca_count: metadata.client_ca_count,
            generation: 1,
            loaded_at: now,
        };
        metrics.set_tls_initial_state(1, now);
        Ok(Self {
            spec,
            config,
            acceptor: DynamicTlsAcceptor::new(initial_acceptor),
            logger,
            metrics,
            reload_lock: Mutex::new(()),
            state: Mutex::new(TlsReloaderState {
                reload_count: 0,
                failure_count: 0,
                last_reload_at: None,
                last_trigger: None,
                last_attempt_at: None,
                last_error: None,
                active,
                last_loaded_digests: digests,
                last_attempted_digests: None,
            }),
        })
    }

    pub fn acceptor(&self) -> DynamicTlsAcceptor {
        self.acceptor.clone()
    }

    pub fn generation(&self) -> u64 {
        self.state
            .lock()
            .expect("tls reloader state lock poisoned")
            .active
            .generation
    }

    pub fn status(&self) -> TlsReloadStatus {
        let state = self.state.lock().expect("tls reloader state lock poisoned");
        Self::status_from_state(&self.config, &state)
    }

    fn status_from_state(config: &TlsReloadConfig, state: &TlsReloaderState) -> TlsReloadStatus {
        TlsReloadStatus {
            enabled: config.enabled,
            watch_interval_ms: config.watch_interval_ms,
            debounce_ms: config.debounce_ms,
            reload_count: state.reload_count,
            failure_count: state.failure_count,
            last_reload_at: state.last_reload_at,
            last_trigger: state.last_trigger.clone(),
            last_attempt_at: state.last_attempt_at,
            last_error: state.last_error.clone(),
            active: state.active.clone(),
        }
    }

    /// Re-read certificates/keys/CA bundle from disk, validate atomically,
    /// and swap the active `TlsAcceptor` without disturbing existing connections.
    pub fn reload(&self, trigger: &str) -> Result<TlsReloadStatus> {
        let _guard = self
            .reload_lock
            .lock()
            .expect("tls reload serialization lock poisoned");
        let now = unix_timestamp();
        match build_tls_acceptor_with_snapshot(&self.spec) {
            Ok((new_acceptor, metadata, digests)) => {
                self.acceptor.swap(new_acceptor);
                let status = {
                    let mut state = self.state.lock().expect("tls reloader state lock poisoned");
                    state.reload_count += 1;
                    state.last_reload_at = Some(now);
                    state.last_attempt_at = Some(now);
                    state.last_trigger = Some(trigger.to_string());
                    state.last_error = None;
                    state.last_loaded_digests = digests.clone();
                    state.last_attempted_digests = Some(digests);
                    state.active.generation += 1;
                    state.active.loaded_at = now;
                    state.active.server_cert_sha256 = metadata.server_cert_sha256;
                    state.active.server_cert_chain_len = metadata.server_cert_chain_len;
                    state.active.client_ca_sha256 = metadata.client_ca_sha256;
                    state.active.client_ca_count = metadata.client_ca_count;
                    Self::status_from_state(&self.config, &state)
                };
                self.metrics
                    .record_tls_reload(trigger, true, status.active.generation, now);
                self.logger.log_security(
                    Severity::Info,
                    "tls.reloaded",
                    "success",
                    Some(trigger),
                    None,
                    None,
                    format!(
                        "TLS certificates hot-reloaded (trigger={trigger}, generation={}, cert_sha256={}, client_ca_count={}, client_auth={})",
                        status.active.generation,
                        status.active.server_cert_sha256,
                        status.active.client_ca_count,
                        status.active.client_auth,
                    ),
                );
                Ok(status)
            }
            Err(err) => {
                let err_msg = format!("{err:#}");
                let attempted = read_watched_file_digests(&self.spec).ok();
                let generation = {
                    let mut state = self.state.lock().expect("tls reloader state lock poisoned");
                    state.failure_count += 1;
                    state.last_attempt_at = Some(now);
                    state.last_error = Some(err_msg.clone());
                    if let Some(digests) = attempted {
                        state.last_attempted_digests = Some(digests);
                    }
                    state.active.generation
                };
                self.metrics.record_tls_reload(trigger, false, generation, now);
                self.logger.log_error(
                    "tls.reload_failed",
                    format!("TLS certificate reload failed (trigger={trigger}): {err_msg}"),
                );
                self.logger.log_security(
                    Severity::Error,
                    "tls.reload_failed",
                    "failure",
                    Some(trigger),
                    None,
                    None,
                    format!("TLS certificate hot-reload failed (trigger={trigger}): {err_msg}"),
                );
                Err(err)
            }
        }
    }

    fn check_watched_files(&self) -> Option<WatchedFileDigests> {
        let digests = read_watched_file_digests(&self.spec).ok()?;
        let state = self.state.lock().expect("tls reloader state lock poisoned");
        if digests == state.last_loaded_digests
            || state.last_attempted_digests.as_ref() == Some(&digests)
        {
            None
        } else {
            Some(digests)
        }
    }

    /// Spawn the background TLS reload worker (file watcher + Unix `SIGHUP`).
    pub fn spawn_worker_with_shutdown(
        self: &Arc<Self>,
        mut stop_rx: watch::Receiver<bool>,
    ) -> JoinHandle<()> {
        let reloader = Arc::clone(self);
        tokio::spawn(async move {
            if !reloader.config.enabled || *stop_rx.borrow() {
                return;
            }

            #[cfg(unix)]
            let mut sighup =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()).ok();

            let watch_ms = reloader.config.watch_interval_ms;
            let debounce = Duration::from_millis(reloader.config.debounce_ms);
            let mut watch_interval = (watch_ms > 0).then(|| {
                let mut iv = tokio::time::interval(Duration::from_millis(watch_ms));
                iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                iv
            });

            let mut pending: Option<(WatchedFileDigests, Instant)> = None;

            loop {
                if *stop_rx.borrow() {
                    break;
                }

                enum Event {
                    Stop,
                    Sighup,
                    Poll,
                    Debounce,
                }

                let event = tokio::select! {
                    biased;
                    res = stop_rx.changed() => {
                        if res.is_err() || *stop_rx.borrow() {
                            Event::Stop
                        } else {
                            continue;
                        }
                    }
                    _ = async {
                        #[cfg(unix)]
                        {
                            match sighup.as_mut() {
                                Some(sig) => {
                                    if sig.recv().await.is_none() {
                                        std::future::pending::<()>().await;
                                    }
                                }
                                None => std::future::pending::<()>().await,
                            }
                        }
                        #[cfg(not(unix))]
                        {
                            std::future::pending::<()>().await;
                        }
                    } => Event::Sighup,
                    _ = async {
                        match pending {
                            Some((_, deadline)) => tokio::time::sleep_until(deadline).await,
                            None => std::future::pending::<()>().await,
                        }
                    } => Event::Debounce,
                    _ = async {
                        match watch_interval.as_mut() {
                            Some(iv) => {
                                iv.tick().await;
                            }
                            None => std::future::pending::<()>().await,
                        }
                    } => Event::Poll,
                };

                match event {
                    Event::Stop => break,
                    Event::Sighup => {
                        pending = None;
                        let r = Arc::clone(&reloader);
                        let _ = tokio::task::spawn_blocking(move || r.reload("sighup")).await;
                    }
                    Event::Poll | Event::Debounce => {
                        let r = Arc::clone(&reloader);
                        let changed = tokio::task::spawn_blocking(move || r.check_watched_files())
                            .await
                            .ok()
                            .flatten();
                        let Some(digests) = changed else {
                            pending = None;
                            continue;
                        };
                        if debounce.is_zero() {
                            pending = None;
                            let r = Arc::clone(&reloader);
                            let _ =
                                tokio::task::spawn_blocking(move || r.reload("file_watcher"))
                                    .await;
                            continue;
                        }
                        let now = Instant::now();
                        match &pending {
                            Some((prev_digests, deadline))
                                if *prev_digests == digests && now >= *deadline =>
                            {
                                pending = None;
                                let r = Arc::clone(&reloader);
                                let _ =
                                    tokio::task::spawn_blocking(move || r.reload("file_watcher"))
                                        .await;
                            }
                            Some((prev_digests, _)) if *prev_digests == digests => {}
                            _ => {
                                pending = Some((digests, now + debounce));
                            }
                        }
                    }
                }
            }
        })
    }
}

/// Inspect the negotiated rustls session for a peer client certificate chain.
pub fn extract_client_cert_info(conn: &ServerConnection) -> Option<ClientCertInfo> {
    let certs = conn.peer_certificates()?;
    let leaf = certs.first()?;
    Some(ClientCertInfo {
        sha256: sha256_hex(leaf.as_ref()),
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
        acceptor: impl Into<DynamicTlsAcceptor>,
        logger: crate::logging::EventForwarder,
    ) -> io::Result<Self> {
        let local_addr = listener.local_addr()?;
        let acceptor = acceptor.into();
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
                let acceptor = acceptor.current();
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

    fn generate_ca_and_leaf_certs(
        dir: &Path,
        prefix: &str,
        ca_cn: &str,
        server_cn: &str,
        client_cn: &str,
    ) -> (PathBuf, PathBuf, PathBuf, PathBuf, PathBuf) {
        let openssl = find_openssl();
        let ca_key = dir.join(format!("{prefix}-ca.key"));
        let ca_cert = dir.join(format!("{prefix}-ca.crt"));
        let server_key = dir.join(format!("{prefix}-server.key"));
        let server_csr = dir.join(format!("{prefix}-server.csr"));
        let server_cert = dir.join(format!("{prefix}-server.crt"));
        let server_ext = dir.join(format!("{prefix}-server.ext"));
        let client_key = dir.join(format!("{prefix}-client.key"));
        let client_csr = dir.join(format!("{prefix}-client.csr"));
        let client_cert = dir.join(format!("{prefix}-client.crt"));
        let client_ext = dir.join(format!("{prefix}-client.ext"));

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

        let s = |p: &Path| p.to_str().unwrap().to_string();

        run_openssl(
            &openssl,
            &[
                "req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:prime256v1",
                "-keyout", &s(&ca_key), "-out", &s(&ca_cert), "-days", "7", "-nodes",
                "-subj", &format!("/CN={ca_cn}"),
            ],
        );
        run_openssl(
            &openssl,
            &[
                "req", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:prime256v1",
                "-keyout", &s(&server_key), "-out", &s(&server_csr), "-nodes",
                "-subj", &format!("/CN={server_cn}"),
            ],
        );
        run_openssl(
            &openssl,
            &[
                "x509", "-req", "-in", &s(&server_csr), "-CA", &s(&ca_cert), "-CAkey", &s(&ca_key),
                "-CAcreateserial", "-out", &s(&server_cert), "-days", "7", "-extfile", &s(&server_ext),
            ],
        );
        run_openssl(
            &openssl,
            &[
                "req", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:prime256v1",
                "-keyout", &s(&client_key), "-out", &s(&client_csr), "-nodes",
                "-subj", &format!("/CN={client_cn}"),
            ],
        );
        run_openssl(
            &openssl,
            &[
                "x509", "-req", "-in", &s(&client_csr), "-CA", &s(&ca_cert), "-CAkey", &s(&ca_key),
                "-CAcreateserial", "-out", &s(&client_cert), "-days", "7", "-extfile", &s(&client_ext),
            ],
        );

        (ca_cert, server_cert, server_key, client_cert, client_key)
    }

    #[test]
    fn tls_reload_config_validates_bounds() {
        assert!(TlsReloadConfig::default().validate().is_ok());
        assert!(TlsReloadConfig {
            enabled: true,
            watch_interval_ms: 0,
            debounce_ms: 0,
        }
        .validate()
        .is_ok());
        assert!(TlsReloadConfig {
            enabled: true,
            watch_interval_ms: MAX_TLS_WATCH_INTERVAL_MS + 1,
            debounce_ms: 250,
        }
        .validate()
        .is_err());
        assert!(TlsReloadConfig {
            enabled: true,
            watch_interval_ms: 2000,
            debounce_ms: MAX_TLS_RELOAD_DEBOUNCE_MS + 1,
        }
        .validate()
        .is_err());
    }

    #[tokio::test]
    async fn tls_reloader_rotates_server_cert_and_mtls_ca_without_dropping_active_connections() -> Result<()> {
        let pki = TestPki::generate();
        let dir = TempDir::new("aircd-tls-hot-reload");
        let live_ca = dir.0.join("live-ca.crt");
        let live_cert = dir.0.join("live-server.crt");
        let live_key = dir.0.join("live-server.key");
        std::fs::copy(&pki.ca_cert, &live_ca)?;
        std::fs::copy(&pki.server_cert, &live_cert)?;
        std::fs::copy(&pki.server_key, &live_key)?;

        let db = store::open(":memory:")?;
        let logger = EventForwarder::new(LogConfig {
            stderr_errors: false,
            ..LogConfig::default()
        })?;
        let server = Server::with_logger(db, logger.clone());
        let (_, irc_token) = server.auth().register_principal("irc-live", Some("worker"), None)?;

        let spec = TlsServerConfigSpec {
            cert_path: live_cert.clone(),
            key_path: live_key.clone(),
            client_ca_path: Some(live_ca.clone()),
            client_auth: ClientAuthMode::Required,
        };
        let reloader = Arc::new(TlsReloader::new(
            spec,
            TlsReloadConfig {
                enabled: false,
                watch_interval_ms: 0,
                debounce_ms: 0,
            },
            logger.clone(),
            server.metrics().clone(),
        )?);
        server.set_tls_reloader(reloader.clone());
        let initial_cert_sha = reloader.status().active.server_cert_sha256;

        let irc_probe = std::net::TcpListener::bind("127.0.0.1:0")?;
        let irc_port = irc_probe.local_addr()?.port();
        drop(irc_probe);
        let http_probe = std::net::TcpListener::bind("127.0.0.1:0")?;
        let http_port = http_probe.local_addr()?.port();
        drop(http_probe);

        let mut info = build_runtime_info(&format!("127.0.0.1:{irc_port}"), "127.0.0.1:0", Some(irc_port));
        info.http_tls_port = Some(http_port);
        info.tls_client_auth = "required".to_string();
        info.tls_only = true;
        info.register_key = Some("reload-invite".to_string());

        let irc_task = tokio::spawn({
            let srv = server.clone();
            let acc = reloader.acceptor();
            async move { srv.listen_tls(&format!("127.0.0.1:{irc_port}"), acc).await }
        });
        let http_task = tokio::spawn({
            let srv = server.clone();
            let acc = reloader.acceptor();
            async move { serve_tls(srv, &format!("127.0.0.1:{http_port}"), info, acc).await }
        });
        tokio::time::sleep(Duration::from_millis(80)).await;

        // Open an active IRC mTLS session using CA1 and keep it open across rotation.
        let server_name = ServerName::try_from("localhost").unwrap();
        let client1_connector =
            build_tls_client_connector(&pki.ca_cert, Some((&pki.client_cert, &pki.client_key)));
        let tcp = TcpStream::connect(("127.0.0.1", irc_port)).await?;
        let irc_tls = client1_connector.connect(server_name.clone(), tcp).await?;
        let (irc_read, mut irc_write) = tokio::io::split(irc_tls);
        let mut irc_lines = BufReader::new(irc_read).lines();
        irc_write
            .write_all(
                format!(
                    "CAP REQ :echo-message\r\nPASS {irc_token}\r\nNICK irc-live\r\nUSER irc-live 0 * :live\r\nCAP END\r\nJOIN #ops\r\n"
                )
                .as_bytes(),
            )
            .await?;
        let mut saw_join = false;
        while let Ok(Some(line)) = tokio::time::timeout(Duration::from_secs(3), irc_lines.next_line()).await? {
            if line.contains(" JOIN #ops") {
                saw_join = true;
                break;
            }
        }
        assert!(saw_join, "expected active IRC client to join #ops before reload");

        // Build CA1 reqwest client and verify initial /v1/owner/tls & /healthz.
        let ca1_reqwest = reqwest::Certificate::from_pem(&std::fs::read(&pki.ca_cert)?)?;
        let mut c1_pem = std::fs::read(&pki.client_cert)?;
        c1_pem.push(b'\n');
        c1_pem.extend_from_slice(&std::fs::read(&pki.client_key)?);
        let c1_id = reqwest::Identity::from_pem(&c1_pem)?;
        let http_ca1 = reqwest::Client::builder()
            .add_root_certificate(ca1_reqwest)
            .identity(c1_id)
            .build()?;
        let base = format!("https://127.0.0.1:{http_port}");
        let health: serde_json::Value = http_ca1
            .get(format!("{base}/healthz"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        assert_eq!(health["tls_generation"], 1);

        // Generate CA2 + new server cert/key + new client cert.
        let (ca2_cert, srv2_cert, srv2_key, cli2_cert, cli2_key) =
            generate_ca_and_leaf_certs(&dir.0, "rot2", "Aircd-CA-2", "localhost", "worker-ca2");
        let new_server_der = load_certs(&srv2_cert, "srv2")?;
        let expected_new_sha = sha256_hex(new_server_der[0].as_ref());
        assert_ne!(expected_new_sha, initial_cert_sha);

        // Overwrite live cert, key, and CA bundle on disk with CA2-only.
        std::fs::copy(&ca2_cert, &live_ca)?;
        std::fs::copy(&srv2_cert, &live_cert)?;
        std::fs::copy(&srv2_key, &live_key)?;

        // Unauthorized /v1/owner/tls without owner key is rejected with 403.
        let denied = http_ca1.get(format!("{base}/v1/owner/tls")).send().await?;
        assert_eq!(denied.status(), reqwest::StatusCode::FORBIDDEN);

        // Authorized GET /v1/owner/tls reports generation=1.
        let status_before: serde_json::Value = http_ca1
            .get(format!("{base}/v1/owner/tls"))
            .header("x-aircd-invite", "reload-invite")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        assert_eq!(status_before["tls_enabled"], true);
        assert_eq!(status_before["status"]["active"]["generation"], 1);

        // Trigger hot-reload via HTTPS Owner API (/v1/owner/tls/reload) using existing CA1 client!
        let reload_resp: serde_json::Value = http_ca1
            .post(format!("{base}/v1/owner/tls/reload"))
            .header("x-aircd-invite", "reload-invite")
            .json(&serde_json::json!({ "reason": "cert_rotation_test" }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        assert_eq!(reload_resp["ok"], true);
        assert_eq!(reload_resp["reloaded"], true);
        assert_eq!(reload_resp["status"]["reload_count"], 1);
        assert_eq!(reload_resp["status"]["active"]["generation"], 2);
        assert_eq!(
            reload_resp["status"]["active"]["server_cert_sha256"],
            expected_new_sha
        );

        // 1. Existing IRC TLS connection must STILL be alive and able to send/receive!
        irc_write
            .write_all(b"PRIVMSG #ops :still alive after hot reload\r\n")
            .await?;
        let mut saw_echo = false;
        while let Ok(Some(line)) = tokio::time::timeout(Duration::from_secs(3), irc_lines.next_line()).await? {
            if line.contains("still alive after hot reload") {
                saw_echo = true;
                break;
            }
        }
        assert!(saw_echo, "active IRC TLS connection should remain uninterrupted across reload");

        // 2. New IRC connection with old CA1 client cert is now REJECTED (even if trusting new server CA).
        let old_client_trusting_new_srv =
            build_tls_client_connector(&ca2_cert, Some((&pki.client_cert, &pki.client_key)));
        assert_tls_rejected(
            &old_client_trusting_new_srv,
            server_name.clone(),
            irc_port,
            "removed CA1 client cert",
        )
        .await;

        // 3. New IRC connection with CA2 client cert SUCCEEDS and sees the new server cert!
        let ca2_connector = build_tls_client_connector(&ca2_cert, Some((&cli2_cert, &cli2_key)));
        let tcp2 = TcpStream::connect(("127.0.0.1", irc_port)).await?;
        let tls2 = ca2_connector.connect(server_name, tcp2).await?;
        let presented_sha = sha256_hex(tls2.get_ref().1.peer_certificates().unwrap()[0].as_ref());
        assert_eq!(presented_sha, expected_new_sha);

        // 4. New HTTPS connection with CA2 client cert succeeds and sees generation=2 & metrics!
        let ca2_reqwest = reqwest::Certificate::from_pem(&std::fs::read(&ca2_cert)?)?;
        let mut c2_pem = std::fs::read(&cli2_cert)?;
        c2_pem.push(b'\n');
        c2_pem.extend_from_slice(&std::fs::read(&cli2_key)?);
        let c2_id = reqwest::Identity::from_pem(&c2_pem)?;
        let http_ca2 = reqwest::Client::builder()
            .add_root_certificate(ca2_reqwest)
            .identity(c2_id)
            .build()?;
        let health2: serde_json::Value = http_ca2
            .get(format!("{base}/healthz"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        assert_eq!(health2["tls_generation"], 2);

        let metrics_body = http_ca2
            .get(format!("{base}/metrics"))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        assert!(metrics_body.contains("aircd_tls_reloads_total{trigger=\"owner_api\",result=\"ok\"} 1"));
        assert!(metrics_body.contains("aircd_tls_reload_generation 2"));

        let (audit_events, _, _) = logger.audit_subscribe(None, Some(50));
        assert!(audit_events.iter().any(|ev| ev.action == "tls.reloaded" && ev.outcome.as_deref() == Some("success")));

        irc_task.abort();
        http_task.abort();
        Ok(())
    }

    #[tokio::test]
    async fn tls_reloader_preserves_active_acceptor_on_invalid_or_mismatched_files() -> Result<()> {
        let pki = TestPki::generate();
        let dir = TempDir::new("aircd-tls-invalid-reload");
        let live_ca = dir.0.join("live-ca.crt");
        let live_cert = dir.0.join("live-server.crt");
        let live_key = dir.0.join("live-server.key");
        std::fs::copy(&pki.ca_cert, &live_ca)?;
        std::fs::copy(&pki.server_cert, &live_cert)?;
        std::fs::copy(&pki.server_key, &live_key)?;

        let logger = EventForwarder::new(LogConfig {
            stderr_errors: false,
            ..LogConfig::default()
        })?;
        let metrics = Arc::new(MetricsRegistry::new());
        let spec = TlsServerConfigSpec {
            cert_path: live_cert.clone(),
            key_path: live_key.clone(),
            client_ca_path: Some(live_ca.clone()),
            client_auth: ClientAuthMode::Required,
        };
        let reloader = Arc::new(TlsReloader::new(
            spec,
            TlsReloadConfig::default(),
            logger.clone(),
            metrics.clone(),
        )?);
        let initial_sha = reloader.status().active.server_cert_sha256;

        // Corrupt the certificate file.
        std::fs::write(&live_cert, b"not a valid pem")?;
        assert!(reloader.reload("owner_api").is_err());

        // Also test mismatched cert & key (cert2 + key1).
        let (_, srv2_cert, _, _, _) =
            generate_ca_and_leaf_certs(&dir.0, "mismatch", "Mismatch-CA", "localhost", "cli");
        std::fs::copy(&srv2_cert, &live_cert)?;
        assert!(reloader.reload("owner_api").is_err());

        let status = reloader.status();
        assert_eq!(status.reload_count, 0);
        assert_eq!(status.failure_count, 2);
        assert_eq!(status.active.generation, 1);
        assert_eq!(status.active.server_cert_sha256, initial_sha);
        assert!(status.last_error.is_some());

        // Existing dynamic acceptor still handshakes with original cert!
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let dyn_acc = reloader.acceptor();
        let srv_task = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            dyn_acc.current().accept(tcp).await.is_ok()
        });
        let trusted =
            build_tls_client_connector(&pki.ca_cert, Some((&pki.client_cert, &pki.client_key)));
        let tcp = TcpStream::connect(("127.0.0.1", port)).await?;
        assert!(trusted
            .connect(ServerName::try_from("localhost").unwrap(), tcp)
            .await
            .is_ok());
        assert!(srv_task.await?);

        let (audit_events, _, _) = logger.audit_subscribe(None, Some(50));
        assert!(audit_events.iter().any(|ev| ev.action == "tls.reload_failed" && ev.outcome.as_deref() == Some("failure")));
        Ok(())
    }

    #[tokio::test]
    async fn tls_reloader_file_watcher_automatically_reloads_and_recovers() -> Result<()> {
        let pki = TestPki::generate();
        let dir = TempDir::new("aircd-tls-watcher");
        let live_ca = dir.0.join("live-ca.crt");
        let live_cert = dir.0.join("live-server.crt");
        let live_key = dir.0.join("live-server.key");
        std::fs::copy(&pki.ca_cert, &live_ca)?;
        std::fs::copy(&pki.server_cert, &live_cert)?;
        std::fs::copy(&pki.server_key, &live_key)?;

        let logger = EventForwarder::new(LogConfig {
            stderr_errors: false,
            ..LogConfig::default()
        })?;
        let metrics = Arc::new(MetricsRegistry::new());
        let spec = TlsServerConfigSpec {
            cert_path: live_cert.clone(),
            key_path: live_key.clone(),
            client_ca_path: Some(live_ca.clone()),
            client_auth: ClientAuthMode::Required,
        };
        let reloader = Arc::new(TlsReloader::new(
            spec,
            TlsReloadConfig {
                enabled: true,
                watch_interval_ms: 40,
                debounce_ms: 40,
            },
            logger,
            metrics,
        )?);

        let (stop_tx, stop_rx) = watch::channel(false);
        let worker = reloader.spawn_worker_with_shutdown(stop_rx);

        let (ca2_cert, srv2_cert, srv2_key, _, _) =
            generate_ca_and_leaf_certs(&dir.0, "watch2", "Watch-CA-2", "localhost", "watch-cli");
        let expected_sha = sha256_hex(load_certs(&srv2_cert, "srv2")?[0].as_ref());

        // Step 1: simulate partial/mismatched write (new cert with old key).
        std::fs::copy(&srv2_cert, &live_cert)?;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(reloader.generation(), 1);
        assert!(reloader.status().failure_count >= 1);

        // Step 2: complete rotation with matching key and new CA.
        std::fs::copy(&srv2_key, &live_key)?;
        std::fs::copy(&ca2_cert, &live_ca)?;

        let mut reloaded = false;
        for _ in 0..40 {
            if reloader.generation() == 2 {
                reloaded = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(reloaded, "file watcher did not hot-reload within timeout");
        let status = reloader.status();
        assert_eq!(status.active.server_cert_sha256, expected_sha);
        assert_eq!(status.last_trigger.as_deref(), Some("file_watcher"));
        assert!(status.last_error.is_none());

        let _ = stop_tx.send(true);
        worker.await?;
        Ok(())
    }

    #[tokio::test]
    async fn owner_tls_endpoints_report_disabled_when_tls_not_configured() -> Result<()> {
        let db = store::open(":memory:")?;
        let logger = EventForwarder::new(LogConfig {
            stderr_errors: false,
            ..LogConfig::default()
        })?;
        let server = Server::with_logger(db, logger);
        let probe = std::net::TcpListener::bind("127.0.0.1:0")?;
        let port = probe.local_addr()?.port();
        drop(probe);
        let bind_addr = format!("127.0.0.1:{port}");
        let mut info = build_runtime_info("127.0.0.1:0", &bind_addr, None);
        info.register_key = Some("owner-secret".to_string());
        let task = tokio::spawn(async move {
            crate::http::serve_with_shutdown(
                server,
                &bind_addr,
                info,
                crate::shutdown::never_stop_rx(),
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(80)).await;

        let client = reqwest::Client::new();
        let base = format!("http://127.0.0.1:{port}");
        let status: serde_json::Value = client
            .get(format!("{base}/v1/owner/tls"))
            .header("x-aircd-invite", "owner-secret")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        assert_eq!(status["ok"], true);
        assert_eq!(status["tls_enabled"], false);
        assert!(status["status"].is_null());

        let reload_resp = client
            .post(format!("{base}/v1/owner/tls/reload"))
            .header("x-aircd-invite", "owner-secret")
            .send()
            .await?;
        assert_eq!(reload_resp.status(), reqwest::StatusCode::BAD_REQUEST);

        task.abort();
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn tls_reloader_sighup_triggers_reload() -> Result<()> {
        let pki = TestPki::generate();
        let dir = TempDir::new("aircd-tls-sighup");
        let live_cert = dir.0.join("live-server.crt");
        let live_key = dir.0.join("live-server.key");
        std::fs::copy(&pki.server_cert, &live_cert)?;
        std::fs::copy(&pki.server_key, &live_key)?;

        let logger = EventForwarder::new(LogConfig {
            stderr_errors: false,
            ..LogConfig::default()
        })?;
        let metrics = Arc::new(MetricsRegistry::new());
        let reloader = Arc::new(TlsReloader::new(
            TlsServerConfigSpec {
                cert_path: live_cert.clone(),
                key_path: live_key.clone(),
                client_ca_path: None,
                client_auth: ClientAuthMode::None,
            },
            TlsReloadConfig {
                enabled: true,
                watch_interval_ms: 0,
                debounce_ms: 0,
            },
            logger,
            metrics,
        )?);

        let (stop_tx, stop_rx) = watch::channel(false);
        let worker = reloader.spawn_worker_with_shutdown(stop_rx);
        tokio::time::sleep(Duration::from_millis(50)).await;

        let (_, srv2_cert, srv2_key, _, _) =
            generate_ca_and_leaf_certs(&dir.0, "hup", "Hup-CA", "localhost", "hup-cli");
        let expected_sha = sha256_hex(load_certs(&srv2_cert, "srv2")?[0].as_ref());
        std::fs::copy(&srv2_cert, &live_cert)?;
        std::fs::copy(&srv2_key, &live_key)?;

        let pid = std::process::id().to_string();
        let out = std::process::Command::new("kill")
            .args(["-HUP", &pid])
            .output()?;
        assert!(out.status.success());

        let mut reloaded = false;
        for _ in 0..40 {
            if reloader.generation() == 2 {
                reloaded = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(reloaded, "SIGHUP did not trigger TLS hot-reload");
        assert_eq!(reloader.status().active.server_cert_sha256, expected_sha);
        assert_eq!(reloader.status().last_trigger.as_deref(), Some("sighup"));

        let _ = stop_tx.send(true);
        worker.await?;
        Ok(())
    }
}
