//! Editable server settings and shared per-user configuration discovery.

use crate::backup::{default_owner_key_path, BackupConfig};
use crate::db::{
    SqlitePoolConfig, SqliteSynchronous, DEFAULT_BUSY_TIMEOUT_MS, DEFAULT_CACHE_SIZE_KIB,
    DEFAULT_DB_POOL_SIZE, DEFAULT_JOURNAL_SIZE_LIMIT_BYTES, DEFAULT_WAL_AUTOCHECKPOINT,
};
use crate::logging::{
    validate_event_log_source, LogConfig, SyslogFacility, SyslogFormat, SyslogTarget,
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
    pub register_key: Option<String>,
    pub open_registration: bool,
    pub auth: AuthConfig,
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
            register_key: None,
            open_registration: false,
            auth: AuthConfig::default(),
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
        })
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
                | "--windows-event-log"
                | "--backup-enabled"
                | "--backup-on-startup" => "true",
                "--no-backups" => "false",
                "--config"
                | "--bind"
                | "--http-bind"
                | "--db"
                | "--tls-cert"
                | "--tls-key"
                | "--tls-bind"
                | "--register-key"
                | "--token-ttl-seconds"
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
                | "--db-journal-size-limit" => iter
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
                    &mut config.backup_dir,
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
            ("AIRCD_DB", "--db"),
            ("AIRCD_TLS_CERT", "--tls-cert"),
            ("AIRCD_TLS_KEY", "--tls-key"),
            ("AIRCD_TLS_BIND", "--tls-bind"),
            ("AIRCD_REGISTER_KEY", "--register-key"),
            ("AIRCD_OPEN_REGISTRATION", "--open-registration"),
            ("AIRCD_TOKEN_TTL_SECONDS", "--token-ttl-seconds"),
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
            ("register_key", &config.register_key),
            ("syslog_target", &config.syslog_target),
            ("syslog_facility", &config.syslog_facility),
            ("backup_dir", &config.backup_dir),
        ] {
            if value.as_ref().is_some_and(|s| s.trim().is_empty()) {
                bail!("server setting {name} must not be empty");
            }
        }
        if config.auth.token_ttl_seconds == Some(0) {
            bail!("server setting auth.token_ttl_seconds must be at least 1");
        }
        let _ = config.to_log_config()?;
        let _ = config.to_backup_config()?;
        let _ = config.to_sqlite_config()?;
        Ok(config)
    }

    fn apply(&mut self, name: &str, value: &str) -> Result<()> {
        match name {
            "--config" => {}
            "--bind" => self.bind = value.into(),
            "--http-bind" => self.http_bind = value.into(),
            "--db" => self.db = value.into(),
            "--tls-cert" => self.tls_cert = Some(value.into()),
            "--tls-key" => self.tls_key = Some(value.into()),
            "--tls-bind" => self.tls_bind = value.into(),
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
            _ => unreachable!(),
        }
        Ok(())
    }
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
}
