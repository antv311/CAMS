//! Editable server settings and shared per-user configuration discovery.

use crate::alerts::AlertConfig;
use crate::backup::{default_owner_key_path, BackupConfig};
use crate::db::{
    SqlitePoolConfig, SqliteSynchronous, DEFAULT_BUSY_TIMEOUT_MS, DEFAULT_CACHE_SIZE_KIB,
    DEFAULT_DB_POOL_SIZE, DEFAULT_JOURNAL_SIZE_LIMIT_BYTES, DEFAULT_WAL_AUTOCHECKPOINT,
};
use crate::logging::{
    validate_event_log_source, LogConfig, SyslogFacility, SyslogFormat, SyslogTarget,
};
use crate::rate_limit::RateLimitConfig;
use crate::shutdown::ShutdownConfig;
use crate::tls::{
    ClientAuthMode, TlsReloadConfig, TlsServerConfigSpec, DEFAULT_TLS_RELOAD_DEBOUNCE_MS,
    DEFAULT_TLS_RELOAD_ENABLED, DEFAULT_TLS_WATCH_INTERVAL_MS,
};
use crate::vacuum::{
    VacuumConfig, DEFAULT_VACUUM_BATCH_PAGES, DEFAULT_VACUUM_ENABLED,
    DEFAULT_VACUUM_INTERVAL_SECS, DEFAULT_VACUUM_MAX_BATCHES_PER_RUN,
    DEFAULT_VACUUM_MIN_FREELIST_PAGES, DEFAULT_VACUUM_MIN_FREELIST_RATIO,
    DEFAULT_VACUUM_STEP_DELAY_MS,
};
use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

pub fn config_dir() -> PathBuf {
    resolve_config_dir(cfg!(windows), |name| std::env::var(name).ok())
}

fn resolve_config_dir(windows: bool, env: impl Fn(&str) -> Option<String>) -> PathBuf {
    let value = |name| {
        env(name)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    if let Some(dir) = value("AIRCD_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    if windows {
        if let Some(dir) = value("LOCALAPPDATA") {
            return PathBuf::from(dir).join("aircd");
        }
    }
    if let Some(dir) = value("XDG_CONFIG_HOME") {
        return PathBuf::from(dir).join("aircd");
    }
    PathBuf::from(
        value("HOME")
            .or_else(|| value("USERPROFILE"))
            .unwrap_or_else(|| ".".into()),
    )
    .join(".config")
    .join("aircd")
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct AuthConfig {
    pub token_ttl_seconds: Option<u64>,
}

// Deliberately no Debug: this contains the optional owner invite credential.
#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub bind: String,
    pub http_bind: String,
    pub db: String,
    pub tls_cert: Option<String>,
    pub tls_key: Option<String>,
    pub tls_bind: String,
    pub tls_client_ca: Option<String>,
    pub tls_client_auth: Option<String>,
    pub http_tls_bind: Option<String>,
    pub tls_only: bool,
    pub tls_reload_enabled: bool,
    pub tls_watch_interval_ms: u64,
    pub tls_reload_debounce_ms: u64,
    pub register_key: Option<String>,
    pub open_registration: bool,
    pub auth: AuthConfig,
    pub rate_limit: RateLimitConfig,
    pub task_timeout: crate::tasks::TaskTimeoutConfig,
    pub windows_event_log: bool,
    pub windows_event_log_source: String,
    pub syslog_target: Option<String>,
    pub syslog_format: String,
    pub syslog_facility: Option<String>,
    pub backup_enabled: bool,
    pub backup_dir: Option<String>,
    pub backup_interval_secs: u64,
    pub backup_retention: usize,
    pub backup_on_startup: bool,
    pub db_pool_size: usize,
    pub db_busy_timeout_ms: u64,
    pub db_synchronous: String,
    pub db_wal_autocheckpoint: u32,
    pub db_cache_size_kib: i64,
    pub db_journal_size_limit_bytes: i64,
    pub db_vacuum_enabled: bool,
    pub db_vacuum_interval_secs: u64,
    pub db_vacuum_min_freelist_pages: u64,
    pub db_vacuum_min_freelist_ratio: f64,
    pub db_vacuum_batch_pages: u32,
    pub db_vacuum_step_delay_ms: u64,
    pub db_vacuum_max_batches_per_run: usize,
    pub alerts: AlertConfig,
    pub shutdown: ShutdownConfig,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:6667".into(),
            http_bind: "127.0.0.1:6680".into(),
            // Preserve the existing working-directory DB default.
            db: "aircd.sqlite3".into(),
            tls_cert: None,
            tls_key: None,
            tls_bind: "127.0.0.1:6697".into(),
            tls_client_ca: None,
            tls_client_auth: None,
            http_tls_bind: None,
            tls_only: false,
            tls_reload_enabled: DEFAULT_TLS_RELOAD_ENABLED,
            tls_watch_interval_ms: DEFAULT_TLS_WATCH_INTERVAL_MS,
            tls_reload_debounce_ms: DEFAULT_TLS_RELOAD_DEBOUNCE_MS,
            register_key: None,
            open_registration: false,
            auth: AuthConfig::default(),
            rate_limit: RateLimitConfig::default(),
            task_timeout: crate::tasks::TaskTimeoutConfig::default(),
            windows_event_log: false,
            windows_event_log_source: "aircd".into(),
            syslog_target: None,
            syslog_format: "rfc5424".into(),
            syslog_facility: None,
            backup_enabled: true,
            backup_dir: None,
            backup_interval_secs: 3600,
            backup_retention: 10,
            backup_on_startup: false,
            db_pool_size: DEFAULT_DB_POOL_SIZE,
            db_busy_timeout_ms: DEFAULT_BUSY_TIMEOUT_MS,
            db_synchronous: "normal".into(),
            db_wal_autocheckpoint: DEFAULT_WAL_AUTOCHECKPOINT,
            db_cache_size_kib: DEFAULT_CACHE_SIZE_KIB,
            db_journal_size_limit_bytes: DEFAULT_JOURNAL_SIZE_LIMIT_BYTES,
            db_vacuum_enabled: DEFAULT_VACUUM_ENABLED,
            db_vacuum_interval_secs: DEFAULT_VACUUM_INTERVAL_SECS,
            db_vacuum_min_freelist_pages: DEFAULT_VACUUM_MIN_FREELIST_PAGES,
            db_vacuum_min_freelist_ratio: DEFAULT_VACUUM_MIN_FREELIST_RATIO,
            db_vacuum_batch_pages: DEFAULT_VACUUM_BATCH_PAGES,
            db_vacuum_step_delay_ms: DEFAULT_VACUUM_STEP_DELAY_MS,
            db_vacuum_max_batches_per_run: DEFAULT_VACUUM_MAX_BATCHES_PER_RUN,
            alerts: AlertConfig::default(),
            shutdown: ShutdownConfig::default(),
        }
    }
}

impl ServerConfig {
    pub fn load(args: &[String]) -> Result<Self> {
        Self::load_with(
            args,
            |name| std::env::var(name).ok(),
            &config_dir().join("aircd.toml"),
        )
    }

    pub fn to_log_config(&self) -> Result<LogConfig> {
        let windows_event_log_source = validate_event_log_source(&self.windows_event_log_source)?;
        let syslog_target = self
            .syslog_target
            .as_deref()
            .map(SyslogTarget::parse)
            .transpose()?;
        let syslog_format = SyslogFormat::parse(&self.syslog_format)?;
        let syslog_facility = self
            .syslog_facility
            .as_deref()
            .map(SyslogFacility::parse_optional)
            .transpose()?
            .flatten();
        Ok(LogConfig {
            windows_event_log: self.windows_event_log,
            windows_event_log_source,
            syslog_target,
            syslog_format,
            syslog_facility,
            stderr_errors: true,
        })
    }

    pub fn to_backup_config(&self) -> Result<BackupConfig> {
        if self.backup_retention == 0 {
            bail!("server setting backup_retention must be at least 1");
        }
        Ok(BackupConfig {
            enabled: self.backup_enabled,
            db_path: self.db.clone(),
            backup_dir: self.backup_dir.as_deref().map(PathBuf::from),
            interval_secs: self.backup_interval_secs,
            retention: self.backup_retention,
            on_startup: self.backup_on_startup,
            owner_key_path: default_owner_key_path(&self.db),
            tls_cert_path: self.tls_cert.as_deref().map(PathBuf::from),
            tls_key_path: self.tls_key.as_deref().map(PathBuf::from),
            tls_client_ca_path: self.tls_client_ca.as_deref().map(PathBuf::from),
        })
    }

    pub fn effective_tls_client_auth(&self) -> Result<ClientAuthMode> {
        let mode = match self.tls_client_auth.as_deref() {
            Some(raw) => Some(ClientAuthMode::parse(raw)?),
            None => None,
        };
        match (mode, self.tls_client_ca.as_deref()) {
            (Some(ClientAuthMode::None), Some(_)) => {
                bail!("tls_client_auth cannot be 'none' when tls_client_ca is set");
            }
            (Some(ClientAuthMode::Optional | ClientAuthMode::Required), None) => {
                bail!("tls_client_ca must be set when tls_client_auth is enabled");
            }
            (Some(explicit), _) => Ok(explicit),
            (None, Some(_)) => Ok(ClientAuthMode::Required),
            (None, None) => Ok(ClientAuthMode::None),
        }
    }

    pub fn to_tls_config(&self) -> Result<Option<TlsServerConfigSpec>> {
        let client_auth = self.effective_tls_client_auth()?;
        match (&self.tls_cert, &self.tls_key) {
            (Some(cert), Some(key)) => Ok(Some(TlsServerConfigSpec {
                cert_path: PathBuf::from(cert),
                key_path: PathBuf::from(key),
                client_ca_path: self.tls_client_ca.as_deref().map(PathBuf::from),
                client_auth,
            })),
            (None, None) => Ok(None),
            _ => bail!("both tls_cert and tls_key must be set to enable TLS"),
        }
    }

    pub fn to_tls_reload_config(&self) -> Result<TlsReloadConfig> {
        let cfg = TlsReloadConfig {
            enabled: self.tls_reload_enabled,
            watch_interval_ms: self.tls_watch_interval_ms,
            debounce_ms: self.tls_reload_debounce_ms,
        };
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn to_sqlite_config(&self) -> Result<SqlitePoolConfig> {
        let cfg = SqlitePoolConfig {
            read_pool_size: self.db_pool_size,
            busy_timeout_ms: self.db_busy_timeout_ms,
            synchronous: SqliteSynchronous::parse(&self.db_synchronous)?,
            wal_autocheckpoint: self.db_wal_autocheckpoint,
            cache_size_kib: self.db_cache_size_kib,
            journal_size_limit_bytes: self.db_journal_size_limit_bytes,
        };
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn to_vacuum_config(&self) -> Result<VacuumConfig> {
        let cfg = VacuumConfig {
            enabled: self.db_vacuum_enabled,
            interval_secs: self.db_vacuum_interval_secs,
            min_freelist_pages: self.db_vacuum_min_freelist_pages,
            min_freelist_ratio: self.db_vacuum_min_freelist_ratio,
            batch_pages: self.db_vacuum_batch_pages,
            step_delay_ms: self.db_vacuum_step_delay_ms,
            max_batches_per_run: self.db_vacuum_max_batches_per_run,
        };
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn to_alert_config(&self) -> Result<AlertConfig> {
        let mut cfg = self.alerts.clone();
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn to_shutdown_config(&self) -> Result<ShutdownConfig> {
        let cfg = self.shutdown.clone();
        cfg.validate()?;
        Ok(cfg)
    }

    fn load_with(
        args: &[String],
        env: impl Fn(&str) -> Option<String>,
        default_path: &Path,
    ) -> Result<Self> {
        let mut options = Vec::new();
        let mut iter = args.iter();
        while let Some(arg) = iter.next() {
            let value = match arg.as_str() {
                "--open-registration"
                | "--tls-only"
                | "--tls-reload-enabled"
                | "--windows-event-log"
                | "--backup-enabled"
                | "--backup-on-startup"
                | "--db-vacuum-enabled"
                | "--rate-limit-enabled"
                | "--rate-limit-trust-proxy-headers"
                | "--alerts-enabled"
                | "--alerts-send-resolved"
                | "--alerts-wake-failure-enabled"
                | "--alerts-queue-backlog-enabled"
                | "--alerts-disk-pressure-enabled"
                | "--shutdown-checkpoint-leases" => "true",
                "--no-backups"
                | "--no-tls-reload"
                | "--no-db-vacuum"
                | "--no-rate-limit"
                | "--no-alerts"
                | "--no-alerts-send-resolved"
                | "--no-alerts-wake-failure"
                | "--no-alerts-queue-backlog"
                | "--no-alerts-disk-pressure"
                | "--no-shutdown-checkpoint-leases" => "false",
                "--config"
                | "--bind"
                | "--http-bind"
                | "--http-tls-bind"
                | "--db"
                | "--tls-cert"
                | "--tls-key"
                | "--tls-bind"
                | "--tls-client-ca"
                | "--tls-client-auth"
                | "--tls-watch-interval-ms"
                | "--tls-reload-debounce-ms"
                | "--register-key"
                | "--token-ttl-seconds"
                | "--rate-limit-ip-rps"
                | "--rate-limit-ip-burst"
                | "--rate-limit-principal-rps"
                | "--rate-limit-principal-burst"
                | "--rate-limit-max-buckets"
                | "--windows-event-log-source"
                | "--syslog-target"
                | "--syslog-format"
                | "--syslog-facility"
                | "--backup-dir"
                | "--backup-interval-secs"
                | "--backup-retention"
                | "--db-pool-size"
                | "--db-busy-timeout-ms"
                | "--db-synchronous"
                | "--db-wal-autocheckpoint"
                | "--db-cache-size-kib"
                | "--db-journal-size-limit"
                | "--db-vacuum-interval-secs"
                | "--db-vacuum-min-freelist-pages"
                | "--db-vacuum-min-freelist-ratio"
                | "--db-vacuum-batch-pages"
                | "--db-vacuum-step-delay-ms"
                | "--db-vacuum-max-batches"
                | "--alerts-eval-interval-secs"
                | "--alerts-webhook-url"
                | "--alerts-webhook-secret"
                | "--alerts-repeat-interval-secs"
                | "--alerts-group-name"
                | "--alerts-wake-failure-rate-threshold"
                | "--alerts-wake-failure-window-secs"
                | "--alerts-wake-failure-min-samples"
                | "--alerts-wake-failure-for-secs"
                | "--alerts-wake-failure-severity"
                | "--alerts-queue-backlog-threshold"
                | "--alerts-queue-backlog-for-secs"
                | "--alerts-queue-backlog-severity"
                | "--alerts-disk-used-ratio-threshold"
                | "--alerts-disk-min-free-bytes"
                | "--alerts-disk-pressure-for-secs"
                | "--alerts-disk-pressure-severity"
                | "--alerts-disk-path"
                | "--drain-timeout-secs"
                | "--http-drain-timeout-secs"
                | "--lease-checkpoint-extension-secs" => iter
                    .next()
                    .filter(|value| !value.starts_with("--"))
                    .with_context(|| format!("{arg} requires a value"))?,
                _ => bail!("unknown argument (use --help)"),
            };
            options.push((arg.as_str(), value));
        }
        let explicit_path = options
            .iter()
            .rev()
            .find(|(name, _)| *name == "--config")
            .map(|(_, path)| path.to_string())
            .or_else(|| env("AIRCD_CONFIG").filter(|s| !s.trim().is_empty()));
        let path = explicit_path
            .as_deref()
            .map(Path::new)
            .unwrap_or(default_path);
        let mut config = match std::fs::read_to_string(path) {
            Ok(raw) => {
                // TOML's normal diagnostic includes source lines, which can
                // contain credentials. Never include those lines in errors.
                let mut config: Self = toml::from_str(&raw).map_err(|_| {
                    anyhow!("parse server config {}: invalid TOML, unknown setting, or wrong value type", path.display())
                })?;
                let parent = path.parent().unwrap_or_else(|| Path::new("."));
                let relative = |value: &mut String| {
                    if !value.trim().is_empty()
                        && value != ":memory:"
                        && Path::new(value).is_relative()
                    {
                        *value = parent.join(&*value).to_string_lossy().into_owned();
                    }
                };
                relative(&mut config.db);
                for value in [
                    &mut config.tls_cert,
                    &mut config.tls_key,
                    &mut config.tls_client_ca,
                    &mut config.backup_dir,
                    &mut config.alerts.disk_path,
                ]
                .into_iter()
                .flatten()
                {
                    relative(value);
                }
                config
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound && explicit_path.is_none() => {
                Self::default()
            }
            Err(err) => {
                return Err(err).with_context(|| format!("read server config {}", path.display()))
            }
        };
        for (name, flag) in [
            ("AIRCD_BIND", "--bind"),
            ("AIRCD_HTTP_BIND", "--http-bind"),
            ("AIRCD_HTTP_TLS_BIND", "--http-tls-bind"),
            ("AIRCD_DB", "--db"),
            ("AIRCD_TLS_CERT", "--tls-cert"),
            ("AIRCD_TLS_KEY", "--tls-key"),
            ("AIRCD_TLS_BIND", "--tls-bind"),
            ("AIRCD_TLS_CLIENT_CA", "--tls-client-ca"),
            ("AIRCD_TLS_CLIENT_AUTH", "--tls-client-auth"),
            ("AIRCD_TLS_ONLY", "--tls-only"),
            ("AIRCD_TLS_RELOAD_ENABLED", "--tls-reload-enabled"),
            ("AIRCD_TLS_WATCH_INTERVAL_MS", "--tls-watch-interval-ms"),
            ("AIRCD_TLS_RELOAD_DEBOUNCE_MS", "--tls-reload-debounce-ms"),
            ("AIRCD_REGISTER_KEY", "--register-key"),
            ("AIRCD_OPEN_REGISTRATION", "--open-registration"),
            ("AIRCD_TOKEN_TTL_SECONDS", "--token-ttl-seconds"),
            ("AIRCD_RATE_LIMIT_ENABLED", "--rate-limit-enabled"),
            ("AIRCD_RATE_LIMIT_IP_RPS", "--rate-limit-ip-rps"),
            ("AIRCD_RATE_LIMIT_IP_BURST", "--rate-limit-ip-burst"),
            ("AIRCD_RATE_LIMIT_PRINCIPAL_RPS", "--rate-limit-principal-rps"),
            ("AIRCD_RATE_LIMIT_PRINCIPAL_BURST", "--rate-limit-principal-burst"),
            (
                "AIRCD_RATE_LIMIT_TRUST_PROXY_HEADERS",
                "--rate-limit-trust-proxy-headers",
            ),
            ("AIRCD_RATE_LIMIT_MAX_BUCKETS", "--rate-limit-max-buckets"),
            ("AIRCD_WINDOWS_EVENT_LOG", "--windows-event-log"),
            ("AIRCD_WINDOWS_EVENT_LOG_SOURCE", "--windows-event-log-source"),
            ("AIRCD_SYSLOG_TARGET", "--syslog-target"),
            ("AIRCD_SYSLOG_FORMAT", "--syslog-format"),
            ("AIRCD_SYSLOG_FACILITY", "--syslog-facility"),
            ("AIRCD_BACKUP_ENABLED", "--backup-enabled"),
            ("AIRCD_BACKUP_DIR", "--backup-dir"),
            ("AIRCD_BACKUP_INTERVAL_SECS", "--backup-interval-secs"),
            ("AIRCD_BACKUP_RETENTION", "--backup-retention"),
            ("AIRCD_BACKUP_ON_STARTUP", "--backup-on-startup"),
            ("AIRCD_DB_POOL_SIZE", "--db-pool-size"),
            ("AIRCD_DB_BUSY_TIMEOUT_MS", "--db-busy-timeout-ms"),
            ("AIRCD_DB_SYNCHRONOUS", "--db-synchronous"),
            ("AIRCD_DB_WAL_AUTOCHECKPOINT", "--db-wal-autocheckpoint"),
            ("AIRCD_DB_CACHE_SIZE_KIB", "--db-cache-size-kib"),
            ("AIRCD_DB_JOURNAL_SIZE_LIMIT", "--db-journal-size-limit"),
            ("AIRCD_DB_VACUUM_ENABLED", "--db-vacuum-enabled"),
            ("AIRCD_DB_VACUUM_INTERVAL_SECS", "--db-vacuum-interval-secs"),
            ("AIRCD_DB_VACUUM_MIN_FREELIST_PAGES", "--db-vacuum-min-freelist-pages"),
            ("AIRCD_DB_VACUUM_MIN_FREELIST_RATIO", "--db-vacuum-min-freelist-ratio"),
            ("AIRCD_DB_VACUUM_BATCH_PAGES", "--db-vacuum-batch-pages"),
            ("AIRCD_DB_VACUUM_STEP_DELAY_MS", "--db-vacuum-step-delay-ms"),
            ("AIRCD_DB_VACUUM_MAX_BATCHES", "--db-vacuum-max-batches"),
            ("AIRCD_ALERTS_ENABLED", "--alerts-enabled"),
            ("AIRCD_ALERTS_EVAL_INTERVAL_SECS", "--alerts-eval-interval-secs"),
            ("AIRCD_ALERTS_WEBHOOK_URL", "--alerts-webhook-url"),
            ("AIRCD_ALERTS_WEBHOOK_SECRET", "--alerts-webhook-secret"),
            ("AIRCD_ALERTS_REPEAT_INTERVAL_SECS", "--alerts-repeat-interval-secs"),
            ("AIRCD_ALERTS_SEND_RESOLVED", "--alerts-send-resolved"),
            ("AIRCD_ALERTS_GROUP_NAME", "--alerts-group-name"),
            ("AIRCD_ALERTS_WAKE_FAILURE_ENABLED", "--alerts-wake-failure-enabled"),
            ("AIRCD_ALERTS_WAKE_FAILURE_RATE_THRESHOLD", "--alerts-wake-failure-rate-threshold"),
            ("AIRCD_ALERTS_WAKE_FAILURE_WINDOW_SECS", "--alerts-wake-failure-window-secs"),
            ("AIRCD_ALERTS_WAKE_FAILURE_MIN_SAMPLES", "--alerts-wake-failure-min-samples"),
            ("AIRCD_ALERTS_WAKE_FAILURE_FOR_SECS", "--alerts-wake-failure-for-secs"),
            ("AIRCD_ALERTS_WAKE_FAILURE_SEVERITY", "--alerts-wake-failure-severity"),
            ("AIRCD_ALERTS_QUEUE_BACKLOG_ENABLED", "--alerts-queue-backlog-enabled"),
            ("AIRCD_ALERTS_QUEUE_BACKLOG_THRESHOLD", "--alerts-queue-backlog-threshold"),
            ("AIRCD_ALERTS_QUEUE_BACKLOG_FOR_SECS", "--alerts-queue-backlog-for-secs"),
            ("AIRCD_ALERTS_QUEUE_BACKLOG_SEVERITY", "--alerts-queue-backlog-severity"),
            ("AIRCD_ALERTS_DISK_PRESSURE_ENABLED", "--alerts-disk-pressure-enabled"),
            ("AIRCD_ALERTS_DISK_USED_RATIO_THRESHOLD", "--alerts-disk-used-ratio-threshold"),
            ("AIRCD_ALERTS_DISK_MIN_FREE_BYTES", "--alerts-disk-min-free-bytes"),
            ("AIRCD_ALERTS_DISK_PRESSURE_FOR_SECS", "--alerts-disk-pressure-for-secs"),
            ("AIRCD_ALERTS_DISK_PRESSURE_SEVERITY", "--alerts-disk-pressure-severity"),
            ("AIRCD_ALERTS_DISK_PATH", "--alerts-disk-path"),
            ("AIRCD_DRAIN_TIMEOUT_SECS", "--drain-timeout-secs"),
            ("AIRCD_HTTP_DRAIN_TIMEOUT_SECS", "--http-drain-timeout-secs"),
            (
                "AIRCD_SHUTDOWN_CHECKPOINT_LEASES",
                "--shutdown-checkpoint-leases",
            ),
            (
                "AIRCD_LEASE_CHECKPOINT_EXTENSION_SECS",
                "--lease-checkpoint-extension-secs",
            ),
        ] {
            if let Some(value) = env(name) {
                // Preserve the existing empty invite-key environment fallback.
                if name == "AIRCD_REGISTER_KEY" && value.trim().is_empty() {
                    continue;
                }
                config
                    .apply(flag, &value)
                    .with_context(|| format!("invalid {name}"))?;
            }
        }
        for (name, value) in options {
            config.apply(name, value)?;
        }
        for (name, value) in [
            ("bind", &config.bind),
            ("http_bind", &config.http_bind),
            ("db", &config.db),
            ("tls_bind", &config.tls_bind),
            ("windows_event_log_source", &config.windows_event_log_source),
            ("syslog_format", &config.syslog_format),
        ] {
            if value.trim().is_empty() {
                bail!("server setting {name} must not be empty");
            }
        }
        if config.tls_cert.is_some() != config.tls_key.is_some() {
            bail!("both tls_cert and tls_key must be set to enable TLS");
        }
        for (name, value) in [
            ("tls_cert", &config.tls_cert),
            ("tls_key", &config.tls_key),
            ("tls_client_ca", &config.tls_client_ca),
            ("tls_client_auth", &config.tls_client_auth),
            ("http_tls_bind", &config.http_tls_bind),
            ("register_key", &config.register_key),
            ("syslog_target", &config.syslog_target),
            ("syslog_facility", &config.syslog_facility),
            ("backup_dir", &config.backup_dir),
        ] {
            if value.as_ref().is_some_and(|s| s.trim().is_empty()) {
                bail!("server setting {name} must not be empty");
            }
        }
        if config.tls_client_ca.is_some()
            && (config.tls_cert.is_none() || config.tls_key.is_none())
        {
            bail!("tls_cert and tls_key must be set when tls_client_ca is configured");
        }
        if config.http_tls_bind.is_some()
            && (config.tls_cert.is_none() || config.tls_key.is_none())
        {
            bail!("tls_cert and tls_key must be set when http_tls_bind is configured");
        }
        if config.tls_only {
            if config.tls_cert.is_none() || config.tls_key.is_none() {
                bail!("tls_cert and tls_key must be set when tls_only is enabled");
            }
            if config.http_tls_bind.is_none() {
                bail!("http_tls_bind must be set when tls_only is enabled");
            }
        }
        let _ = config.effective_tls_client_auth()?;
        if config.auth.token_ttl_seconds == Some(0) {
            bail!("server setting auth.token_ttl_seconds must be at least 1");
        }
        config.rate_limit.validate()?;
        let _ = config.to_log_config()?;
        let _ = config.to_backup_config()?;
        let _ = config.to_sqlite_config()?;
        let _ = config.to_vacuum_config()?;
        let _ = config.to_tls_reload_config()?;
        config.alerts.validate()?;
        config.shutdown.validate()?;
        Ok(config)
    }

    fn apply(&mut self, name: &str, value: &str) -> Result<()> {
        match name {
            "--config" => {}
            "--bind" => self.bind = value.into(),
            "--http-bind" => self.http_bind = value.into(),
            "--http-tls-bind" => self.http_tls_bind = Some(value.into()),
            "--db" => self.db = value.into(),
            "--tls-cert" => self.tls_cert = Some(value.into()),
            "--tls-key" => self.tls_key = Some(value.into()),
            "--tls-bind" => self.tls_bind = value.into(),
            "--tls-client-ca" => self.tls_client_ca = Some(value.into()),
            "--tls-client-auth" => self.tls_client_auth = Some(value.into()),
            "--tls-only" => {
                self.tls_only = parse_bool_setting("tls_only", value)?;
            }
            "--tls-reload-enabled" | "--no-tls-reload" => {
                self.tls_reload_enabled = parse_bool_setting("tls_reload_enabled", value)?;
            }
            "--tls-watch-interval-ms" => {
                self.tls_watch_interval_ms = parse_u64_setting("tls_watch_interval_ms", value)?;
            }
            "--tls-reload-debounce-ms" => {
                self.tls_reload_debounce_ms =
                    parse_u64_setting("tls_reload_debounce_ms", value)?;
            }
            "--register-key" => self.register_key = Some(value.into()),
            "--open-registration" => {
                self.open_registration = parse_bool_setting("open_registration", value)?;
            }
            "--token-ttl-seconds" => {
                let ttl = value
                    .trim()
                    .parse::<u64>()
                    .with_context(|| format!("token_ttl_seconds must be a positive integer, got '{value}'"))?;
                if ttl == 0 {
                    bail!("server setting auth.token_ttl_seconds must be at least 1");
                }
                self.auth.token_ttl_seconds = Some(ttl);
            }
            "--rate-limit-enabled" | "--no-rate-limit" => {
                self.rate_limit.enabled = parse_bool_setting("rate_limit.enabled", value)?;
            }
            "--rate-limit-ip-rps" => {
                self.rate_limit.ip_requests_per_sec = value.trim().parse::<f64>().with_context(|| {
                    format!("rate_limit.ip_requests_per_sec must be a number, got '{value}'")
                })?;
            }
            "--rate-limit-ip-burst" => {
                self.rate_limit.ip_burst = value.trim().parse::<u32>().with_context(|| {
                    format!("rate_limit.ip_burst must be a non-negative integer, got '{value}'")
                })?;
            }
            "--rate-limit-principal-rps" => {
                self.rate_limit.principal_requests_per_sec =
                    value.trim().parse::<f64>().with_context(|| {
                        format!(
                            "rate_limit.principal_requests_per_sec must be a number, got '{value}'"
                        )
                    })?;
            }
            "--rate-limit-principal-burst" => {
                self.rate_limit.principal_burst = value.trim().parse::<u32>().with_context(|| {
                    format!("rate_limit.principal_burst must be a non-negative integer, got '{value}'")
                })?;
            }
            "--rate-limit-trust-proxy-headers" => {
                self.rate_limit.trust_proxy_headers =
                    parse_bool_setting("rate_limit.trust_proxy_headers", value)?;
            }
            "--rate-limit-max-buckets" => {
                self.rate_limit.max_buckets = value.trim().parse::<usize>().with_context(|| {
                    format!("rate_limit.max_buckets must be a positive integer, got '{value}'")
                })?;
            }
            "--windows-event-log" => {
                self.windows_event_log = parse_bool_setting("windows_event_log", value)?;
            }
            "--windows-event-log-source" => self.windows_event_log_source = value.into(),
            "--syslog-target" => self.syslog_target = Some(value.into()),
            "--syslog-format" => self.syslog_format = value.into(),
            "--syslog-facility" => self.syslog_facility = Some(value.into()),
            "--backup-enabled" | "--no-backups" => {
                self.backup_enabled = parse_bool_setting("backup_enabled", value)?;
            }
            "--backup-dir" => self.backup_dir = Some(value.into()),
            "--backup-interval-secs" => {
                self.backup_interval_secs = value
                    .trim()
                    .parse::<u64>()
                    .with_context(|| format!("backup_interval_secs must be a non-negative integer, got '{value}'"))?;
            }
            "--backup-retention" => {
                self.backup_retention = value
                    .trim()
                    .parse::<usize>()
                    .with_context(|| format!("backup_retention must be a positive integer, got '{value}'"))?;
            }
            "--backup-on-startup" => {
                self.backup_on_startup = parse_bool_setting("backup_on_startup", value)?;
            }
            "--db-pool-size" => {
                self.db_pool_size = value
                    .trim()
                    .parse::<usize>()
                    .with_context(|| format!("db_pool_size must be a positive integer, got '{value}'"))?;
            }
            "--db-busy-timeout-ms" => {
                self.db_busy_timeout_ms = value
                    .trim()
                    .parse::<u64>()
                    .with_context(|| format!("db_busy_timeout_ms must be a non-negative integer, got '{value}'"))?;
            }
            "--db-synchronous" => {
                self.db_synchronous = value.into();
            }
            "--db-wal-autocheckpoint" => {
                self.db_wal_autocheckpoint = value
                    .trim()
                    .parse::<u32>()
                    .with_context(|| format!("db_wal_autocheckpoint must be a non-negative integer, got '{value}'"))?;
            }
            "--db-cache-size-kib" => {
                self.db_cache_size_kib = value
                    .trim()
                    .parse::<i64>()
                    .with_context(|| format!("db_cache_size_kib must be a non-negative integer, got '{value}'"))?;
            }
            "--db-journal-size-limit" => {
                self.db_journal_size_limit_bytes = value
                    .trim()
                    .parse::<i64>()
                    .with_context(|| format!("db_journal_size_limit_bytes must be an integer >= -1, got '{value}'"))?;
            }
            "--db-vacuum-enabled" | "--no-db-vacuum" => {
                self.db_vacuum_enabled = parse_bool_setting("db_vacuum_enabled", value)?;
            }
            "--db-vacuum-interval-secs" => {
                self.db_vacuum_interval_secs = value
                    .trim()
                    .parse::<u64>()
                    .with_context(|| format!("db_vacuum_interval_secs must be a non-negative integer, got '{value}'"))?;
            }
            "--db-vacuum-min-freelist-pages" => {
                self.db_vacuum_min_freelist_pages = value
                    .trim()
                    .parse::<u64>()
                    .with_context(|| format!("db_vacuum_min_freelist_pages must be a non-negative integer, got '{value}'"))?;
            }
            "--db-vacuum-min-freelist-ratio" => {
                self.db_vacuum_min_freelist_ratio = value
                    .trim()
                    .parse::<f64>()
                    .with_context(|| format!("db_vacuum_min_freelist_ratio must be a number in 0.0..=1.0, got '{value}'"))?;
            }
            "--db-vacuum-batch-pages" => {
                self.db_vacuum_batch_pages = value
                    .trim()
                    .parse::<u32>()
                    .with_context(|| format!("db_vacuum_batch_pages must be a positive integer, got '{value}'"))?;
            }
            "--db-vacuum-step-delay-ms" => {
                self.db_vacuum_step_delay_ms = value
                    .trim()
                    .parse::<u64>()
                    .with_context(|| format!("db_vacuum_step_delay_ms must be a non-negative integer, got '{value}'"))?;
            }
            "--db-vacuum-max-batches" => {
                self.db_vacuum_max_batches_per_run = value
                    .trim()
                    .parse::<usize>()
                    .with_context(|| format!("db_vacuum_max_batches_per_run must be a non-negative integer, got '{value}'"))?;
            }
            "--alerts-enabled" | "--no-alerts" => {
                self.alerts.enabled = parse_bool_setting("alerts.enabled", value)?;
            }
            "--alerts-eval-interval-secs" => {
                self.alerts.eval_interval_secs =
                    parse_u64_setting("alerts.eval_interval_secs", value)?;
            }
            "--alerts-webhook-url" => self.alerts.webhook_url = Some(value.into()),
            "--alerts-webhook-secret" => self.alerts.webhook_secret = Some(value.into()),
            "--alerts-repeat-interval-secs" => {
                self.alerts.repeat_interval_secs =
                    parse_u64_setting("alerts.repeat_interval_secs", value)?;
            }
            "--alerts-send-resolved" | "--no-alerts-send-resolved" => {
                self.alerts.send_resolved = parse_bool_setting("alerts.send_resolved", value)?;
            }
            "--alerts-group-name" => self.alerts.group_name = value.into(),
            "--alerts-wake-failure-enabled" | "--no-alerts-wake-failure" => {
                self.alerts.wake_failure_enabled =
                    parse_bool_setting("alerts.wake_failure_enabled", value)?;
            }
            "--alerts-wake-failure-rate-threshold" => {
                self.alerts.wake_failure_rate_threshold =
                    parse_f64_setting("alerts.wake_failure_rate_threshold", value)?;
            }
            "--alerts-wake-failure-window-secs" => {
                self.alerts.wake_failure_window_secs =
                    parse_u64_setting("alerts.wake_failure_window_secs", value)?;
            }
            "--alerts-wake-failure-min-samples" => {
                self.alerts.wake_failure_min_samples =
                    parse_u64_setting("alerts.wake_failure_min_samples", value)?;
            }
            "--alerts-wake-failure-for-secs" => {
                self.alerts.wake_failure_for_secs =
                    parse_u64_setting("alerts.wake_failure_for_secs", value)?;
            }
            "--alerts-wake-failure-severity" => {
                self.alerts.wake_failure_severity = value.into();
            }
            "--alerts-queue-backlog-enabled" | "--no-alerts-queue-backlog" => {
                self.alerts.queue_backlog_enabled =
                    parse_bool_setting("alerts.queue_backlog_enabled", value)?;
            }
            "--alerts-queue-backlog-threshold" => {
                self.alerts.queue_backlog_threshold =
                    parse_u64_setting("alerts.queue_backlog_threshold", value)?;
            }
            "--alerts-queue-backlog-for-secs" => {
                self.alerts.queue_backlog_for_secs =
                    parse_u64_setting("alerts.queue_backlog_for_secs", value)?;
            }
            "--alerts-queue-backlog-severity" => {
                self.alerts.queue_backlog_severity = value.into();
            }
            "--alerts-disk-pressure-enabled" | "--no-alerts-disk-pressure" => {
                self.alerts.disk_pressure_enabled =
                    parse_bool_setting("alerts.disk_pressure_enabled", value)?;
            }
            "--alerts-disk-used-ratio-threshold" => {
                self.alerts.disk_used_ratio_threshold =
                    parse_f64_setting("alerts.disk_used_ratio_threshold", value)?;
            }
            "--alerts-disk-min-free-bytes" => {
                self.alerts.disk_min_free_bytes =
                    parse_u64_setting("alerts.disk_min_free_bytes", value)?;
            }
            "--alerts-disk-pressure-for-secs" => {
                self.alerts.disk_pressure_for_secs =
                    parse_u64_setting("alerts.disk_pressure_for_secs", value)?;
            }
            "--alerts-disk-pressure-severity" => {
                self.alerts.disk_pressure_severity = value.into();
            }
            "--alerts-disk-path" => self.alerts.disk_path = Some(value.into()),
            "--drain-timeout-secs" => {
                self.shutdown.drain_timeout_secs =
                    parse_u64_setting("shutdown.drain_timeout_secs", value)?;
            }
            "--http-drain-timeout-secs" => {
                self.shutdown.http_drain_timeout_secs =
                    parse_u64_setting("shutdown.http_drain_timeout_secs", value)?;
            }
            "--shutdown-checkpoint-leases" | "--no-shutdown-checkpoint-leases" => {
                self.shutdown.checkpoint_leases_on_drain =
                    parse_bool_setting("shutdown.checkpoint_leases_on_drain", value)?;
            }
            "--lease-checkpoint-extension-secs" => {
                self.shutdown.lease_checkpoint_extension_secs =
                    parse_u64_setting("shutdown.lease_checkpoint_extension_secs", value)?;
            }
            _ => unreachable!(),
        }
        Ok(())
    }
}

fn parse_u64_setting(name: &str, value: &str) -> Result<u64> {
    value
        .trim()
        .parse::<u64>()
        .with_context(|| format!("{name} must be a non-negative integer, got '{value}'"))
}

fn parse_f64_setting(name: &str, value: &str) -> Result<f64> {
    value
        .trim()
        .parse::<f64>()
        .with_context(|| format!("{name} must be a number, got '{value}'"))
}

fn parse_bool_setting(name: &str, value: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" => Ok(true),
        "0" | "false" | "no" | "" => Ok(false),
        _ => bail!("{name} must be true/false, yes/no, or 1/0"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn environment<'a>(pairs: &'a [(&str, &str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        }
    }

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!("aircd-config-{}", uuid::Uuid::new_v4())))
        }
        fn write(&self, raw: &str) -> PathBuf {
            std::fs::create_dir_all(&self.0).unwrap();
            let path = self.0.join("aircd.toml");
            std::fs::write(&path, raw).unwrap();
            path
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn config_directory_matches_client_precedence_and_skips_blank_values() {
        let env = environment(&[
            ("AIRCD_CONFIG_DIR", " custom "),
            ("LOCALAPPDATA", "local"),
            ("XDG_CONFIG_HOME", "xdg"),
            ("HOME", "home"),
        ]);
        assert_eq!(resolve_config_dir(true, env), PathBuf::from("custom"));
        let env = environment(&[
            ("AIRCD_CONFIG_DIR", " "),
            ("LOCALAPPDATA", "local"),
            ("XDG_CONFIG_HOME", "xdg"),
            ("HOME", "home"),
        ]);
        assert_eq!(
            resolve_config_dir(true, &env),
            Path::new("local").join("aircd")
        );
        assert_eq!(
            resolve_config_dir(false, &env),
            Path::new("xdg").join("aircd")
        );
        assert_eq!(
            resolve_config_dir(
                false,
                environment(&[("HOME", " "), ("USERPROFILE", "profile")])
            ),
            Path::new("profile").join(".config/aircd")
        );
        assert_eq!(
            resolve_config_dir(false, |_| None),
            Path::new(".").join(".config/aircd")
        );
    }

    #[test]
    fn missing_default_file_preserves_existing_defaults() -> Result<()> {
        let fixture = Fixture::new();
        let cfg = ServerConfig::load_with(&[], |_| None, &fixture.0.join("missing"))?;
        assert_eq!(cfg.bind, "127.0.0.1:6667");
        assert_eq!(cfg.http_bind, "127.0.0.1:6680");
        assert_eq!(cfg.db, "aircd.sqlite3");
        assert!(!cfg.open_registration);
        assert!(cfg.register_key.is_none());
        Ok(())
    }

    #[test]
    fn file_settings_resolve_paths_beside_config_and_env_and_flags_override() -> Result<()> {
        let fixture = Fixture::new();
        let path = fixture.write("bind = '127.0.0.1:7001'\nhttp_bind = '127.0.0.1:7002'\ndb = 'hub.sqlite3'\ntls_cert = 'cert.pem'\ntls_key = 'key.pem'\nregister_key = 'fixture-only'\nopen_registration = true\n");
        let cfg = ServerConfig::load_with(&[], |_| None, &path)?;
        assert_eq!(cfg.bind, "127.0.0.1:7001");
        assert_eq!(Path::new(&cfg.db), fixture.0.join("hub.sqlite3"));
        assert_eq!(
            Path::new(cfg.tls_cert.as_deref().unwrap()),
            fixture.0.join("cert.pem")
        );
        assert_eq!(
            Path::new(cfg.tls_key.as_deref().unwrap()),
            fixture.0.join("key.pem")
        );
        assert_eq!(cfg.register_key.as_deref(), Some("fixture-only"));
        assert!(cfg.open_registration);
        let env = environment(&[
            ("AIRCD_BIND", "127.0.0.1:8001"),
            ("AIRCD_HTTP_BIND", "127.0.0.1:8002"),
            ("AIRCD_DB", "env.sqlite3"),
            ("AIRCD_OPEN_REGISTRATION", "false"),
        ]);
        let cfg = ServerConfig::load_with(
            &[
                "--bind".into(),
                "127.0.0.1:9001".into(),
                "--db".into(),
                "cli.sqlite3".into(),
            ],
            env,
            &path,
        )?;
        assert_eq!(cfg.bind, "127.0.0.1:9001");
        assert_eq!(cfg.http_bind, "127.0.0.1:8002");
        assert_eq!(cfg.db, "cli.sqlite3");
        assert!(!cfg.open_registration);
        Ok(())
    }

    #[test]
    fn explicit_config_flag_overrides_environment_and_missing_explicit_file_fails() -> Result<()> {
        let fixture = Fixture::new();
        let path = fixture.write("db = ':memory:'\n");
        let env_path = fixture.0.join("absent").to_string_lossy().into_owned();
        let env_values = [("AIRCD_CONFIG", env_path.as_str())];
        let env = environment(&env_values);
        let cfg = ServerConfig::load_with(
            &["--config".into(), path.to_string_lossy().into_owned()],
            &env,
            &path,
        )?;
        assert_eq!(cfg.db, ":memory:");
        assert!(ServerConfig::load_with(&[], env, &path).is_err());
        let cfg = ServerConfig::load_with(
            &[],
            environment(&[("AIRCD_CONFIG", path.to_str().unwrap())]),
            &fixture.0.join("absent"),
        )?;
        assert_eq!(cfg.db, ":memory:");
        Ok(())
    }

    #[test]
    fn invalid_or_unknown_file_settings_fail_without_echoing_credentials() {
        let fixture = Fixture::new();
        for raw in [
            "register_key = 'fixture-secret'\nunknown = 1",
            "register_key = 12345",
            "register_key = 'fixture-secret'\nbind = [",
        ] {
            let path = fixture.write(raw);
            let err = ServerConfig::load_with(&[], |_| None, &path)
                .err()
                .unwrap()
                .to_string();
            assert!(err.contains("parse server config"));
            assert!(!err.contains("fixture-secret"));
            assert!(!err.contains("12345"));
        }
    }

    #[test]
    fn invalid_empty_credentials_tls_pair_and_missing_flag_values_fail() {
        let fixture = Fixture::new();
        for raw in [
            "register_key = ' '",
            "tls_cert = 'cert.pem'",
            "db = ''",
            "db = ' '",
            "tls_cert = ''\ntls_key = ''",
            "tls_cert = ' '\ntls_key = ' '",
        ] {
            let path = fixture.write(raw);
            assert!(ServerConfig::load_with(&[], |_| None, &path).is_err());
        }
        let path = fixture.write("");
        for args in [
            vec!["--db".into()],
            vec!["--config".into()],
            vec!["--db".into(), "--open-registration".into()],
            vec!["--unknown".into()],
        ] {
            assert!(ServerConfig::load_with(&args, |_| None, &path).is_err());
        }
        assert!(ServerConfig::load_with(
            &[],
            environment(&[("AIRCD_OPEN_REGISTRATION", "invalid")]),
            &path
        )
        .is_err());
    }

    #[test]
    fn example_config_is_loadable_and_uses_safe_defaults() -> Result<()> {
        let fixture = Fixture::new();
        let path = fixture.write(include_str!("../aircd.toml.example"));
        let cfg = ServerConfig::load_with(&[], |_| None, &path)?;
        assert!(!cfg.open_registration);
        assert!(cfg.register_key.is_none());
        assert_eq!(cfg.bind, "127.0.0.1:6667");
        assert!(!cfg.windows_event_log);
        assert_eq!(cfg.windows_event_log_source, "aircd");
        assert!(cfg.syslog_target.is_none());
        assert_eq!(cfg.syslog_format, "rfc5424");
        assert!(cfg.syslog_facility.is_none());
        Ok(())
    }

    #[test]
    fn logging_forwarding_settings_respect_precedence_and_validate() -> Result<()> {
        let fixture = Fixture::new();
        let path = fixture.write(
            "windows_event_log = true\n\
             windows_event_log_source = 'cams-hub'\n\
             syslog_target = 'udp://127.0.0.1:514'\n\
             syslog_format = 'rfc3164'\n\
             syslog_facility = 'local2'\n",
        );
        let cfg = ServerConfig::load_with(&[], |_| None, &path)?;
        let log_cfg = cfg.to_log_config()?;
        assert!(log_cfg.windows_event_log);
        assert_eq!(log_cfg.windows_event_log_source, "cams-hub");
        assert_eq!(
            log_cfg.syslog_target,
            Some(SyslogTarget::Udp("127.0.0.1:514".into()))
        );
        assert_eq!(log_cfg.syslog_format, SyslogFormat::Rfc3164);
        assert_eq!(log_cfg.syslog_facility, Some(SyslogFacility::Local2));

        let env = environment(&[
            ("AIRCD_WINDOWS_EVENT_LOG", "false"),
            ("AIRCD_SYSLOG_TARGET", "tcp://127.0.0.1:6514"),
            ("AIRCD_SYSLOG_FORMAT", "rfc5424"),
            ("AIRCD_SYSLOG_FACILITY", "auto"),
        ]);
        let cfg = ServerConfig::load_with(
            &[
                "--windows-event-log".into(),
                "--windows-event-log-source".into(),
                "aircd-prod".into(),
            ],
            env,
            &path,
        )?;
        let log_cfg = cfg.to_log_config()?;
        assert!(log_cfg.windows_event_log);
        assert_eq!(log_cfg.windows_event_log_source, "aircd-prod");
        assert_eq!(
            log_cfg.syslog_target,
            Some(SyslogTarget::Tcp("127.0.0.1:6514".into()))
        );
        assert_eq!(log_cfg.syslog_format, SyslogFormat::Rfc5424);
        assert_eq!(log_cfg.syslog_facility, None);

        for bad in [
            "syslog_target = 'not-a-host-port'",
            "syslog_format = 'json'",
            "syslog_facility = 'bogus'",
            "windows_event_log_source = ' '",
            "windows_event_log_source = 'bad/source'",
            "backup_retention = 0",
            "backup_dir = ' '",
        ] {
            let bad_path = fixture.write(bad);
            assert!(
                ServerConfig::load_with(&[], |_| None, &bad_path).is_err(),
                "expected invalid config for {bad}"
            );
        }
        Ok(())
    }

    #[test]
    fn backup_settings_respect_precedence_and_relative_paths() -> Result<()> {
        let fixture = Fixture::new();
        let path = fixture.write(
            "db = 'hub.sqlite3'\n\
             backup_enabled = false\n\
             backup_dir = 'snapshots'\n\
             backup_interval_secs = 900\n\
             backup_retention = 5\n\
             backup_on_startup = true\n",
        );
        let cfg = ServerConfig::load_with(&[], |_| None, &path)?;
        let backup_cfg = cfg.to_backup_config()?;
        assert!(!backup_cfg.enabled);
        assert_eq!(backup_cfg.backup_dir, Some(fixture.0.join("snapshots")));
        assert_eq!(backup_cfg.interval_secs, 900);
        assert_eq!(backup_cfg.retention, 5);
        assert!(backup_cfg.on_startup);
        assert_eq!(
            backup_cfg.owner_key_path,
            Some(fixture.0.join("aircd-owner.key"))
        );

        let env = environment(&[
            ("AIRCD_BACKUP_ENABLED", "true"),
            ("AIRCD_BACKUP_INTERVAL_SECS", "120"),
            ("AIRCD_BACKUP_RETENTION", "12"),
            ("AIRCD_BACKUP_ON_STARTUP", "false"),
        ]);
        let cfg = ServerConfig::load_with(
            &[
                "--no-backups".into(),
                "--backup-retention".into(),
                "4".into(),
                "--backup-on-startup".into(),
            ],
            env,
            &path,
        )?;
        let backup_cfg = cfg.to_backup_config()?;
        assert!(!backup_cfg.enabled);
        assert_eq!(backup_cfg.interval_secs, 120);
        assert_eq!(backup_cfg.retention, 4);
        assert!(backup_cfg.on_startup);
        Ok(())
    }

    #[test]
    fn sqlite_settings_respect_precedence_and_validate() -> Result<()> {
        let fixture = Fixture::new();
        let path = fixture.write(
            "db_pool_size = 12\n\
             db_busy_timeout_ms = 8000\n\
             db_synchronous = 'full'\n\
             db_wal_autocheckpoint = 2000\n\
             db_cache_size_kib = 32768\n\
             db_journal_size_limit_bytes = 134217728\n",
        );
        let cfg = ServerConfig::load_with(&[], |_| None, &path)?;
        let sqlite_cfg = cfg.to_sqlite_config()?;
        assert_eq!(sqlite_cfg.read_pool_size, 12);
        assert_eq!(sqlite_cfg.busy_timeout_ms, 8000);
        assert_eq!(sqlite_cfg.synchronous, SqliteSynchronous::Full);
        assert_eq!(sqlite_cfg.wal_autocheckpoint, 2000);
        assert_eq!(sqlite_cfg.cache_size_kib, 32768);
        assert_eq!(sqlite_cfg.journal_size_limit_bytes, 134_217_728);

        let env = environment(&[
            ("AIRCD_DB_POOL_SIZE", "6"),
            ("AIRCD_DB_SYNCHRONOUS", "normal"),
            ("AIRCD_DB_BUSY_TIMEOUT_MS", "4500"),
        ]);
        let cfg = ServerConfig::load_with(
            &[
                "--db-pool-size".into(),
                "16".into(),
                "--db-wal-autocheckpoint".into(),
                "500".into(),
            ],
            env,
            &path,
        )?;
        let sqlite_cfg = cfg.to_sqlite_config()?;
        assert_eq!(sqlite_cfg.read_pool_size, 16);
        assert_eq!(sqlite_cfg.synchronous, SqliteSynchronous::Normal);
        assert_eq!(sqlite_cfg.busy_timeout_ms, 4500);
        assert_eq!(sqlite_cfg.wal_autocheckpoint, 500);

        for bad in [
            "db_pool_size = 0",
            "db_pool_size = 999",
            "db_synchronous = 'bogus'",
            "db_cache_size_kib = -1",
            "db_journal_size_limit_bytes = -2",
            "[auth]\ntoken_ttl_seconds = 0",
        ] {
            let bad_path = fixture.write(bad);
            assert!(
                ServerConfig::load_with(&[], |_| None, &bad_path).is_err(),
                "expected invalid config for {bad}"
            );
        }
        Ok(())
    }

    #[test]
    fn auth_token_ttl_settings_respect_precedence() -> Result<()> {
        let fixture = Fixture::new();
        let path = fixture.write("[auth]\ntoken_ttl_seconds = 3600\n");
        let cfg = ServerConfig::load_with(&[], |_| None, &path)?;
        assert_eq!(cfg.auth.token_ttl_seconds, Some(3600));

        let env = environment(&[("AIRCD_TOKEN_TTL_SECONDS", "7200")]);
        let cfg = ServerConfig::load_with(&[], &env, &path)?;
        assert_eq!(cfg.auth.token_ttl_seconds, Some(7200));

        let cfg = ServerConfig::load_with(
            &["--token-ttl-seconds".into(), "900".into()],
            &env,
            &path,
        )?;
        assert_eq!(cfg.auth.token_ttl_seconds, Some(900));
        Ok(())
    }

    #[test]
    fn rate_limit_settings_respect_precedence_and_validate() -> Result<()> {
        let fixture = Fixture::new();
        let path = fixture.write(
            "[rate_limit]\n\
             enabled = true\n\
             ip_requests_per_sec = 50.0\n\
             ip_burst = 100\n\
             principal_requests_per_sec = 12.5\n\
             principal_burst = 25\n\
             trust_proxy_headers = true\n\
             max_buckets = 2048\n",
        );
        let cfg = ServerConfig::load_with(&[], |_| None, &path)?;
        assert!(cfg.rate_limit.enabled);
        assert_eq!(cfg.rate_limit.ip_requests_per_sec, 50.0);
        assert_eq!(cfg.rate_limit.ip_burst, 100);
        assert_eq!(cfg.rate_limit.principal_requests_per_sec, 12.5);
        assert_eq!(cfg.rate_limit.principal_burst, 25);
        assert!(cfg.rate_limit.trust_proxy_headers);
        assert_eq!(cfg.rate_limit.max_buckets, 2048);

        let env = environment(&[
            ("AIRCD_RATE_LIMIT_ENABLED", "false"),
            ("AIRCD_RATE_LIMIT_IP_RPS", "75"),
            ("AIRCD_RATE_LIMIT_PRINCIPAL_BURST", "40"),
            ("AIRCD_RATE_LIMIT_TRUST_PROXY_HEADERS", "false"),
        ]);
        let cfg = ServerConfig::load_with(
            &[
                "--rate-limit-enabled".into(),
                "--rate-limit-ip-burst".into(),
                "150".into(),
                "--rate-limit-principal-rps".into(),
                "20".into(),
                "--rate-limit-max-buckets".into(),
                "4096".into(),
            ],
            &env,
            &path,
        )?;
        assert!(cfg.rate_limit.enabled);
        assert_eq!(cfg.rate_limit.ip_requests_per_sec, 75.0);
        assert_eq!(cfg.rate_limit.ip_burst, 150);
        assert_eq!(cfg.rate_limit.principal_requests_per_sec, 20.0);
        assert_eq!(cfg.rate_limit.principal_burst, 40);
        assert!(!cfg.rate_limit.trust_proxy_headers);
        assert_eq!(cfg.rate_limit.max_buckets, 4096);

        let cfg = ServerConfig::load_with(&["--no-rate-limit".into()], &env, &path)?;
        assert!(!cfg.rate_limit.enabled);

        for bad in [
            "[rate_limit]\nip_requests_per_sec = -1",
            "[rate_limit]\nip_burst = 0",
            "[rate_limit]\nprincipal_requests_per_sec = -0.5",
            "[rate_limit]\nprincipal_burst = 0",
            "[rate_limit]\nmax_buckets = 0",
            "[rate_limit]\nunknown_key = 1",
        ] {
            let bad_path = fixture.write(bad);
            assert!(
                ServerConfig::load_with(&[], |_| None, &bad_path).is_err(),
                "expected invalid config for {bad}"
            );
        }
        Ok(())
    }

    #[test]
    fn mtls_and_https_settings_respect_precedence_and_validate() -> Result<()> {
        let fixture = Fixture::new();
        let path = fixture.write(
            "tls_cert = 'server.pem'\n\
             tls_key = 'server.key'\n\
             tls_client_ca = 'ca.pem'\n\
             http_tls_bind = '127.0.0.1:6443'\n\
             tls_only = true\n",
        );
        let cfg = ServerConfig::load_with(&[], |_| None, &path)?;
        assert_eq!(
            Path::new(cfg.tls_client_ca.as_deref().unwrap()),
            fixture.0.join("ca.pem")
        );
        assert_eq!(cfg.http_tls_bind.as_deref(), Some("127.0.0.1:6443"));
        assert!(cfg.tls_only);
        assert_eq!(cfg.effective_tls_client_auth()?, ClientAuthMode::Required);
        let tls_spec = cfg.to_tls_config()?.expect("tls spec");
        assert_eq!(tls_spec.client_auth, ClientAuthMode::Required);
        assert_eq!(tls_spec.client_ca_path, Some(fixture.0.join("ca.pem")));

        let env = environment(&[
            ("AIRCD_TLS_CLIENT_AUTH", "optional"),
            ("AIRCD_TLS_ONLY", "false"),
            ("AIRCD_HTTP_TLS_BIND", "0.0.0.0:7443"),
        ]);
        let cfg = ServerConfig::load_with(
            &[
                "--tls-client-auth".into(),
                "required".into(),
                "--tls-only".into(),
            ],
            &env,
            &path,
        )?;
        assert_eq!(cfg.effective_tls_client_auth()?, ClientAuthMode::Required);
        assert_eq!(cfg.http_tls_bind.as_deref(), Some("0.0.0.0:7443"));
        assert!(cfg.tls_only);

        for bad in [
            "tls_client_ca = 'ca.pem'",
            "tls_cert = 'c.pem'\ntls_key = 'k.pem'\ntls_client_auth = 'required'",
            "tls_cert = 'c.pem'\ntls_key = 'k.pem'\ntls_client_ca = 'ca.pem'\ntls_client_auth = 'none'",
            "tls_cert = 'c.pem'\ntls_key = 'k.pem'\ntls_client_ca = 'ca.pem'\ntls_client_auth = 'bogus'",
            "tls_cert = 'c.pem'\ntls_key = 'k.pem'\ntls_client_ca = ' '",
            "http_tls_bind = '127.0.0.1:6443'",
            "tls_cert = 'c.pem'\ntls_key = 'k.pem'\ntls_only = true",
            "tls_only = true",
        ] {
            let bad_path = fixture.write(bad);
            assert!(
                ServerConfig::load_with(&[], |_| None, &bad_path).is_err(),
                "expected invalid mTLS config for {bad}"
            );
        }
        Ok(())
    }

    #[test]
    fn vacuum_settings_respect_precedence_and_validate() -> Result<()> {
        let fixture = Fixture::new();
        let path = fixture.write(
            "db_vacuum_enabled = false\n\
             db_vacuum_interval_secs = 600\n\
             db_vacuum_min_freelist_pages = 128\n\
             db_vacuum_min_freelist_ratio = 0.25\n\
             db_vacuum_batch_pages = 32\n\
             db_vacuum_step_delay_ms = 10\n\
             db_vacuum_max_batches_per_run = 16\n",
        );
        let cfg = ServerConfig::load_with(&[], |_| None, &path)?;
        let vac = cfg.to_vacuum_config()?;
        assert!(!vac.enabled);
        assert_eq!(vac.interval_secs, 600);
        assert_eq!(vac.min_freelist_pages, 128);
        assert_eq!(vac.min_freelist_ratio, 0.25);
        assert_eq!(vac.batch_pages, 32);
        assert_eq!(vac.step_delay_ms, 10);
        assert_eq!(vac.max_batches_per_run, 16);

        let env = environment(&[
            ("AIRCD_DB_VACUUM_ENABLED", "true"),
            ("AIRCD_DB_VACUUM_INTERVAL_SECS", "120"),
            ("AIRCD_DB_VACUUM_BATCH_PAGES", "48"),
        ]);
        let cfg = ServerConfig::load_with(
            &[
                "--db-vacuum-min-freelist-pages".into(),
                "256".into(),
                "--db-vacuum-min-freelist-ratio".into(),
                "0.15".into(),
                "--db-vacuum-step-delay-ms".into(),
                "5".into(),
                "--db-vacuum-max-batches".into(),
                "8".into(),
            ],
            &env,
            &path,
        )?;
        let vac = cfg.to_vacuum_config()?;
        assert!(vac.enabled);
        assert_eq!(vac.interval_secs, 120);
        assert_eq!(vac.min_freelist_pages, 256);
        assert_eq!(vac.min_freelist_ratio, 0.15);
        assert_eq!(vac.batch_pages, 48);
        assert_eq!(vac.step_delay_ms, 5);
        assert_eq!(vac.max_batches_per_run, 8);

        let cfg = ServerConfig::load_with(&["--no-db-vacuum".into()], &env, &path)?;
        assert!(!cfg.to_vacuum_config()?.enabled);

        for bad in [
            "db_vacuum_batch_pages = 0",
            "db_vacuum_min_freelist_ratio = -0.1",
            "db_vacuum_min_freelist_ratio = 1.5",
        ] {
            let bad_path = fixture.write(bad);
            assert!(
                ServerConfig::load_with(&[], |_| None, &bad_path).is_err(),
                "expected invalid vacuum config for {bad}"
            );
        }
        Ok(())
    }

    #[test]
    fn alerts_settings_respect_precedence_and_validate() -> Result<()> {
        let fixture = Fixture::new();
        let path = fixture.write(
            "[alerts]\n\
             enabled = false\n\
             eval_interval_secs = 45\n\
             webhook_url = 'http://127.0.0.1:9095/alerts'\n\
             webhook_secret = 'file-secret'\n\
             repeat_interval_secs = 600\n\
             send_resolved = false\n\
             group_name = 'cams_file'\n\
             wake_failure_rate_threshold = 0.35\n\
             wake_failure_window_secs = 600\n\
             wake_failure_min_samples = 8\n\
             wake_failure_for_secs = 90\n\
             wake_failure_severity = 'warning'\n\
             queue_backlog_threshold = 25\n\
             queue_backlog_for_secs = 30\n\
             disk_used_ratio_threshold = 0.85\n\
             disk_min_free_bytes = 2147483648\n\
             disk_path = 'storage'\n",
        );
        let cfg = ServerConfig::load_with(&[], |_| None, &path)?;
        let alerts = cfg.to_alert_config()?;
        assert!(!alerts.enabled);
        assert_eq!(alerts.eval_interval_secs, 45);
        assert_eq!(
            alerts.webhook_url.as_deref(),
            Some("http://127.0.0.1:9095/alerts")
        );
        assert_eq!(alerts.webhook_secret.as_deref(), Some("file-secret"));
        assert_eq!(alerts.repeat_interval_secs, 600);
        assert!(!alerts.send_resolved);
        assert_eq!(alerts.group_name, "cams_file");
        assert_eq!(alerts.wake_failure_rate_threshold, 0.35);
        assert_eq!(alerts.wake_failure_window_secs, 600);
        assert_eq!(alerts.wake_failure_min_samples, 8);
        assert_eq!(alerts.wake_failure_for_secs, 90);
        assert_eq!(alerts.wake_failure_severity, "warning");
        assert_eq!(alerts.queue_backlog_threshold, 25);
        assert_eq!(alerts.queue_backlog_for_secs, 30);
        assert_eq!(alerts.disk_used_ratio_threshold, 0.85);
        assert_eq!(alerts.disk_min_free_bytes, 2_147_483_648);
        assert_eq!(
            Path::new(alerts.disk_path.as_deref().unwrap()),
            fixture.0.join("storage")
        );

        let env = environment(&[
            ("AIRCD_ALERTS_ENABLED", "true"),
            ("AIRCD_ALERTS_EVAL_INTERVAL_SECS", "20"),
            ("AIRCD_ALERTS_SEND_RESOLVED", "true"),
            ("AIRCD_ALERTS_QUEUE_BACKLOG_THRESHOLD", "40"),
            ("AIRCD_ALERTS_DISK_PRESSURE_SEVERITY", "warning"),
        ]);
        let cfg = ServerConfig::load_with(
            &[
                "--alerts-eval-interval-secs".into(),
                "10".into(),
                "--alerts-wake-failure-rate-threshold".into(),
                "0.5".into(),
                "--alerts-wake-failure-severity".into(),
                "critical".into(),
                "--no-alerts-queue-backlog".into(),
            ],
            &env,
            &path,
        )?;
        let alerts = cfg.to_alert_config()?;
        assert!(alerts.enabled);
        assert_eq!(alerts.eval_interval_secs, 10);
        assert!(alerts.send_resolved);
        assert_eq!(alerts.wake_failure_rate_threshold, 0.5);
        assert_eq!(alerts.wake_failure_severity, "critical");
        assert!(!alerts.queue_backlog_enabled);
        assert_eq!(alerts.queue_backlog_threshold, 40);
        assert_eq!(alerts.disk_pressure_severity, "warning");

        let cfg = ServerConfig::load_with(&["--no-alerts".into()], &env, &path)?;
        assert!(!cfg.to_alert_config()?.enabled);

        for bad in [
            "[alerts]\nwebhook_url = 'http://example.com/public-http'",
            "[alerts]\nwebhook_secret = '   '",
            "[alerts]\ngroup_name = 'has space'",
            "[alerts]\nwake_failure_rate_threshold = 1.5",
            "[alerts]\nwake_failure_window_secs = 0",
            "[alerts]\ndisk_used_ratio_threshold = -0.1",
            "[alerts]\nwake_failure_severity = 'urgent'",
            "[alerts]\nunknown_key = 1",
        ] {
            let bad_path = fixture.write(bad);
            assert!(
                ServerConfig::load_with(&[], |_| None, &bad_path).is_err(),
                "expected invalid alerts config for {bad}"
            );
        }
        Ok(())
    }

    #[test]
    fn shutdown_settings_load_and_validate() -> Result<()> {
        let fixture = Fixture::new();
        let empty = fixture.write("");
        let defaults = ServerConfig::load_with(&[], |_| None, &empty)?.to_shutdown_config()?;
        assert_eq!(defaults, ShutdownConfig::default());

        let path = fixture.write(
            "[shutdown]\n\
             drain_timeout_secs = 45\n\
             http_drain_timeout_secs = 8\n\
             checkpoint_leases_on_drain = false\n\
             lease_checkpoint_extension_secs = 600\n",
        );
        let cfg = ServerConfig::load_with(&[], |_| None, &path)?.to_shutdown_config()?;
        assert_eq!(cfg.drain_timeout_secs, 45);
        assert_eq!(cfg.http_drain_timeout_secs, 8);
        assert!(!cfg.checkpoint_leases_on_drain);
        assert_eq!(cfg.lease_checkpoint_extension_secs, 600);

        let env = environment(&[
            ("AIRCD_DRAIN_TIMEOUT_SECS", "20"),
            ("AIRCD_HTTP_DRAIN_TIMEOUT_SECS", "3"),
            ("AIRCD_SHUTDOWN_CHECKPOINT_LEASES", "true"),
            ("AIRCD_LEASE_CHECKPOINT_EXTENSION_SECS", "450"),
        ]);
        let cfg = ServerConfig::load_with(&[], &env, &path)?.to_shutdown_config()?;
        assert_eq!(cfg.drain_timeout_secs, 20);
        assert_eq!(cfg.http_drain_timeout_secs, 3);
        assert!(cfg.checkpoint_leases_on_drain);
        assert_eq!(cfg.lease_checkpoint_extension_secs, 450);

        let cfg = ServerConfig::load_with(
            &[
                "--drain-timeout-secs".into(),
                "12".into(),
                "--http-drain-timeout-secs".into(),
                "4".into(),
                "--no-shutdown-checkpoint-leases".into(),
                "--lease-checkpoint-extension-secs".into(),
                "180".into(),
            ],
            &env,
            &path,
        )?
        .to_shutdown_config()?;
        assert_eq!(cfg.drain_timeout_secs, 12);
        assert_eq!(cfg.http_drain_timeout_secs, 4);
        assert!(!cfg.checkpoint_leases_on_drain);
        assert_eq!(cfg.lease_checkpoint_extension_secs, 180);

        let cfg = ServerConfig::load_with(
            &["--shutdown-checkpoint-leases".into()],
            &env,
            &path,
        )?
        .to_shutdown_config()?;
        assert!(cfg.checkpoint_leases_on_drain);

        let zero_timeouts = fixture.write(
            "[shutdown]\n\
             drain_timeout_secs = 0\n\
             http_drain_timeout_secs = 0\n",
        );
        let zero_cfg =
            ServerConfig::load_with(&[], |_| None, &zero_timeouts)?.to_shutdown_config()?;
        assert_eq!(zero_cfg.drain_timeout_secs, 0);
        assert_eq!(zero_cfg.http_drain_timeout_secs, 0);

        for bad in [
            "[shutdown]\ndrain_timeout_secs = 90000",
            "[shutdown]\nhttp_drain_timeout_secs = 4000",
            "[shutdown]\nlease_checkpoint_extension_secs = 0",
            "[shutdown]\nlease_checkpoint_extension_secs = 700000",
            "[shutdown]\nunknown_key = 1",
        ] {
            let bad_path = fixture.write(bad);
            assert!(
                ServerConfig::load_with(&[], |_| None, &bad_path).is_err(),
                "expected invalid shutdown config for {bad}"
            );
        }
        Ok(())
    }

    #[test]
    fn tls_reload_settings_load_and_validate() -> Result<()> {
        let fixture = Fixture::new();
        let empty = fixture.write("");
        let defaults = ServerConfig::load_with(&[], |_| None, &empty)?.to_tls_reload_config()?;
        assert!(defaults.enabled);
        assert_eq!(defaults.watch_interval_ms, DEFAULT_TLS_WATCH_INTERVAL_MS);
        assert_eq!(defaults.debounce_ms, DEFAULT_TLS_RELOAD_DEBOUNCE_MS);

        let path = fixture.write(
            "tls_reload_enabled = false\n\
             tls_watch_interval_ms = 5000\n\
             tls_reload_debounce_ms = 500\n",
        );
        let cfg = ServerConfig::load_with(&[], |_| None, &path)?.to_tls_reload_config()?;
        assert!(!cfg.enabled);
        assert_eq!(cfg.watch_interval_ms, 5000);
        assert_eq!(cfg.debounce_ms, 500);

        let env = environment(&[
            ("AIRCD_TLS_RELOAD_ENABLED", "true"),
            ("AIRCD_TLS_WATCH_INTERVAL_MS", "1500"),
            ("AIRCD_TLS_RELOAD_DEBOUNCE_MS", "100"),
        ]);
        let cfg = ServerConfig::load_with(&[], &env, &path)?.to_tls_reload_config()?;
        assert!(cfg.enabled);
        assert_eq!(cfg.watch_interval_ms, 1500);
        assert_eq!(cfg.debounce_ms, 100);

        let cfg = ServerConfig::load_with(
            &[
                "--no-tls-reload".into(),
                "--tls-watch-interval-ms".into(),
                "250".into(),
                "--tls-reload-debounce-ms".into(),
                "50".into(),
            ],
            &env,
            &path,
        )?
        .to_tls_reload_config()?;
        assert!(!cfg.enabled);
        assert_eq!(cfg.watch_interval_ms, 250);
        assert_eq!(cfg.debounce_ms, 50);

        let cfg = ServerConfig::load_with(&["--tls-reload-enabled".into()], &env, &path)?
            .to_tls_reload_config()?;
        assert!(cfg.enabled);

        for bad in [
            "tls_watch_interval_ms = 90000000",
            "tls_reload_debounce_ms = 70000",
        ] {
            let bad_path = fixture.write(bad);
            assert!(
                ServerConfig::load_with(&[], |_| None, &bad_path).is_err(),
                "expected invalid tls reload config for {bad}"
            );
        }
        Ok(())
    }
}
