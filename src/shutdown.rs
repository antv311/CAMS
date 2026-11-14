//! Graceful hub shutdown coordination, OS signal handling, and worker draining.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use tokio::sync::{watch, Notify};

/// Returns a `watch::Receiver<bool>` whose sender is kept alive for the process
/// lifetime and never fires, for legacy callers that do not pass a shutdown channel.
pub fn never_stop_rx() -> watch::Receiver<bool> {
    static NEVER_STOP: OnceLock<watch::Sender<bool>> = OnceLock::new();
    NEVER_STOP.get_or_init(|| watch::channel(false).0).subscribe()
}

pub const DEFAULT_DRAIN_TIMEOUT_SECS: u64 = 30;
pub const DEFAULT_HTTP_DRAIN_TIMEOUT_SECS: u64 = 5;
pub const DEFAULT_CHECKPOINT_LEASES_ON_DRAIN: bool = true;
pub const DEFAULT_LEASE_CHECKPOINT_EXTENSION_SECS: u64 = 300;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ShutdownConfig {
    /// Maximum seconds to wait for running wake executions to complete before
    /// checkpointing remaining leases and detaching workers (`0` skips waiting).
    pub drain_timeout_secs: u64,
    /// Maximum seconds to wait for in-flight HTTP requests to drain after
    /// listeners stop accepting new connections (`0` drops immediately).
    pub http_drain_timeout_secs: u64,
    /// Whether to checkpoint active task leases / execution heartbeats during
    /// graceful shutdown so restarted hubs do not prematurely expire workers.
    pub checkpoint_leases_on_drain: bool,
    /// Seconds to extend `claimed` task leases when checkpointing during drain.
    pub lease_checkpoint_extension_secs: u64,
}

impl Default for ShutdownConfig {
    fn default() -> Self {
        Self {
            drain_timeout_secs: DEFAULT_DRAIN_TIMEOUT_SECS,
            http_drain_timeout_secs: DEFAULT_HTTP_DRAIN_TIMEOUT_SECS,
            checkpoint_leases_on_drain: DEFAULT_CHECKPOINT_LEASES_ON_DRAIN,
            lease_checkpoint_extension_secs: DEFAULT_LEASE_CHECKPOINT_EXTENSION_SECS,
        }
    }
}

impl ShutdownConfig {
    pub fn validate(&self) -> Result<()> {
        if self.drain_timeout_secs > 86_400 {
            bail!("shutdown.drain_timeout_secs must be at most 86400 seconds");
        }
        if self.http_drain_timeout_secs > 3_600 {
            bail!("shutdown.http_drain_timeout_secs must be at most 3600 seconds");
        }
        if self.lease_checkpoint_extension_secs == 0
            || self.lease_checkpoint_extension_secs > 604_800
        {
            bail!("shutdown.lease_checkpoint_extension_secs must be between 1 and 604800 seconds");
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ShutdownPhase {
    #[default]
    Running,
    Draining,
    Detaching,
    Stopped,
}

impl ShutdownPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Draining => "draining",
            Self::Detaching => "detaching",
            Self::Stopped => "stopped",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct DrainReport {
    pub signal: String,
    pub completed_within_timeout: bool,
    pub timed_out: bool,
    pub forced_by_signal: bool,
    pub cancelled_pending_wakes: usize,
    pub initial_active_wakes: usize,
    pub remaining_active_wakes: usize,
    pub detached_wake_runs: usize,
    pub checkpointed_tasks: usize,
    pub detached_irc_clients: usize,
    pub elapsed_ms: u64,
}

#[derive(Clone)]
pub struct ShutdownCoordinator {
    inner: Arc<ShutdownInner>,
}

struct ShutdownInner {
    config: RwLock<ShutdownConfig>,
    phase: Mutex<ShutdownPhase>,
    draining: AtomicBool,
    drain_requested: AtomicBool,
    requested_signal: Mutex<Option<String>>,
    drain_notify: Notify,
    stop_accepting_tx: watch::Sender<bool>,
    last_report: Mutex<Option<DrainReport>>,
}

impl Default for ShutdownCoordinator {
    fn default() -> Self {
        Self::new(ShutdownConfig::default())
    }
}

impl ShutdownCoordinator {
    pub fn new(config: ShutdownConfig) -> Self {
        let (stop_accepting_tx, _) = watch::channel(false);
        Self {
            inner: Arc::new(ShutdownInner {
                config: RwLock::new(config),
                phase: Mutex::new(ShutdownPhase::Running),
                draining: AtomicBool::new(false),
                drain_requested: AtomicBool::new(false),
                requested_signal: Mutex::new(None),
                drain_notify: Notify::new(),
                stop_accepting_tx,
                last_report: Mutex::new(None),
            }),
        }
    }

    pub fn config(&self) -> ShutdownConfig {
        self.inner
            .config
            .read()
            .expect("shutdown config lock poisoned")
            .clone()
    }

    pub fn update_config(&self, config: ShutdownConfig) {
        *self
            .inner
            .config
            .write()
            .expect("shutdown config lock poisoned") = config;
    }

    pub fn phase(&self) -> ShutdownPhase {
        *self
            .inner
            .phase
            .lock()
            .expect("shutdown phase lock poisoned")
    }

    pub fn set_phase(&self, phase: ShutdownPhase) {
        *self
            .inner
            .phase
            .lock()
            .expect("shutdown phase lock poisoned") = phase;
        if phase != ShutdownPhase::Running {
            self.inner.draining.store(true, Ordering::SeqCst);
        }
    }

    pub fn is_draining(&self) -> bool {
        self.inner.draining.load(Ordering::SeqCst)
    }

    /// Request graceful drain from an in-process caller (such as `POST /v1/owner/drain`).
    /// Returns `true` if this call initiated the drain request.
    pub fn request_drain(&self, source: impl Into<String>) -> bool {
        let first = !self.inner.drain_requested.swap(true, Ordering::SeqCst);
        if first {
            *self
                .inner
                .requested_signal
                .lock()
                .expect("shutdown signal lock poisoned") = Some(source.into());
        }
        self.inner.draining.store(true, Ordering::SeqCst);
        self.inner.drain_notify.notify_waiters();
        first
    }

    pub async fn wait_for_drain_request(&self) -> String {
        loop {
            let notified = self.inner.drain_notify.notified();
            if self.inner.drain_requested.load(Ordering::SeqCst) {
                return self
                    .inner
                    .requested_signal
                    .lock()
                    .expect("shutdown signal lock poisoned")
                    .clone()
                    .unwrap_or_else(|| "shutdown_request".to_string());
            }
            notified.await;
        }
    }

    pub fn stop_accepting_rx(&self) -> watch::Receiver<bool> {
        self.inner.stop_accepting_tx.subscribe()
    }

    pub fn trigger_stop_accepting(&self) {
        let _ = self.inner.stop_accepting_tx.send(true);
    }

    pub fn store_report(&self, report: DrainReport) {
        *self
            .inner
            .last_report
            .lock()
            .expect("shutdown report lock poisoned") = Some(report);
    }

    pub fn last_report(&self) -> Option<DrainReport> {
        self.inner
            .last_report
            .lock()
            .expect("shutdown report lock poisoned")
            .clone()
    }
}

/// Cross-platform OS shutdown signal listener supporting Unix `SIGINT`/`SIGTERM`
/// and Windows console/service control events (`CTRL_C`, `CTRL_BREAK`,
/// `CTRL_CLOSE`, `CTRL_SHUTDOWN`).
pub struct ShutdownSignalListener {
    #[cfg(unix)]
    sigint: Option<tokio::signal::unix::Signal>,
    #[cfg(unix)]
    sigterm: Option<tokio::signal::unix::Signal>,
    #[cfg(windows)]
    ctrl_c: Option<tokio::signal::windows::CtrlC>,
    #[cfg(windows)]
    ctrl_break: Option<tokio::signal::windows::CtrlBreak>,
    #[cfg(windows)]
    ctrl_close: Option<tokio::signal::windows::CtrlClose>,
    #[cfg(windows)]
    ctrl_shutdown: Option<tokio::signal::windows::CtrlShutdown>,
}

#[cfg(unix)]
async fn wait_unix_signal(sig: Option<&mut tokio::signal::unix::Signal>) {
    match sig {
        Some(sig) => {
            if sig.recv().await.is_none() {
                std::future::pending::<()>().await;
            }
        }
        None => std::future::pending::<()>().await,
    }
}

#[cfg(windows)]
async fn wait_windows_ctrl<F, Fut>(
    present: bool,
    wait_fn: F,
) where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Option<()>>,
{
    if !present || wait_fn().await.is_none() {
        std::future::pending::<()>().await;
    }
}

impl ShutdownSignalListener {
    pub fn bind() -> Self {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            Self {
                sigint: signal(SignalKind::interrupt()).ok(),
                sigterm: signal(SignalKind::terminate()).ok(),
            }
        }
        #[cfg(windows)]
        {
            use tokio::signal::windows::{ctrl_break, ctrl_c, ctrl_close, ctrl_shutdown};
            Self {
                ctrl_c: ctrl_c().ok(),
                ctrl_break: ctrl_break().ok(),
                ctrl_close: ctrl_close().ok(),
                ctrl_shutdown: ctrl_shutdown().ok(),
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            Self {}
        }
    }

    pub async fn recv(&mut self) -> &'static str {
        #[cfg(unix)]
        {
            let has_any = self.sigint.is_some() || self.sigterm.is_some();
            if !has_any {
                if tokio::signal::ctrl_c().await.is_ok() {
                    return "SIGINT";
                }
                return std::future::pending::<&'static str>().await;
            }
            tokio::select! {
                _ = wait_unix_signal(self.sigint.as_mut()) => "SIGINT",
                _ = wait_unix_signal(self.sigterm.as_mut()) => "SIGTERM",
            }
        }
        #[cfg(windows)]
        {
            let has_any = self.ctrl_c.is_some()
                || self.ctrl_break.is_some()
                || self.ctrl_close.is_some()
                || self.ctrl_shutdown.is_some();
            if !has_any {
                if tokio::signal::ctrl_c().await.is_ok() {
                    return "CTRL_C";
                }
                return std::future::pending::<&'static str>().await;
            }
            let has_c = self.ctrl_c.is_some();
            let has_break = self.ctrl_break.is_some();
            let has_close = self.ctrl_close.is_some();
            let has_shutdown = self.ctrl_shutdown.is_some();
            tokio::select! {
                _ = wait_windows_ctrl(has_c, || async {
                    match self.ctrl_c.as_mut() {
                        Some(sig) => sig.recv().await,
                        None => None,
                    }
                }) => "CTRL_C",
                _ = wait_windows_ctrl(has_break, || async {
                    match self.ctrl_break.as_mut() {
                        Some(sig) => sig.recv().await,
                        None => None,
                    }
                }) => "CTRL_BREAK",
                _ = wait_windows_ctrl(has_close, || async {
                    match self.ctrl_close.as_mut() {
                        Some(sig) => sig.recv().await,
                        None => None,
                    }
                }) => "CTRL_CLOSE",
                _ = wait_windows_ctrl(has_shutdown, || async {
                    match self.ctrl_shutdown.as_mut() {
                        Some(sig) => sig.recv().await,
                        None => None,
                    }
                }) => "CTRL_SHUTDOWN",
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            if tokio::signal::ctrl_c().await.is_ok() {
                return "CTRL_C";
            }
            std::future::pending::<&'static str>().await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_config_defaults_and_validation() {
        let cfg = ShutdownConfig::default();
        assert_eq!(cfg.drain_timeout_secs, 30);
        assert_eq!(cfg.http_drain_timeout_secs, 5);
        assert!(cfg.checkpoint_leases_on_drain);
        assert_eq!(cfg.lease_checkpoint_extension_secs, 300);
        assert!(cfg.validate().is_ok());

        let too_long = ShutdownConfig {
            drain_timeout_secs: 90_000,
            ..Default::default()
        };
        assert!(too_long.validate().is_err());

        let bad_http = ShutdownConfig {
            http_drain_timeout_secs: 4_000,
            ..Default::default()
        };
        assert!(bad_http.validate().is_err());

        let zero_ext = ShutdownConfig {
            lease_checkpoint_extension_secs: 0,
            ..Default::default()
        };
        assert!(zero_ext.validate().is_err());

        let bad_ext = ShutdownConfig {
            lease_checkpoint_extension_secs: 700_000,
            ..Default::default()
        };
        assert!(bad_ext.validate().is_err());

        let rx = never_stop_rx();
        assert!(!*rx.borrow());
    }

    #[tokio::test]
    async fn coordinator_request_drain_notifies_once_and_updates_phase() {
        let coord = ShutdownCoordinator::default();
        assert_eq!(coord.phase(), ShutdownPhase::Running);
        assert!(!coord.is_draining());

        let coord_clone = coord.clone();
        let handle = tokio::spawn(async move { coord_clone.wait_for_drain_request().await });
        tokio::task::yield_now().await;

        assert!(coord.request_drain("SIGTERM"));
        assert!(!coord.request_drain("SIGINT"));
        assert!(coord.is_draining());

        let sig = handle.await.unwrap();
        assert_eq!(sig, "SIGTERM");

        coord.set_phase(ShutdownPhase::Detaching);
        assert_eq!(coord.phase(), ShutdownPhase::Detaching);
        assert_eq!(coord.phase().as_str(), "detaching");
    }
}
