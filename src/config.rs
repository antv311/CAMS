//! Editable server settings and shared per-user configuration discovery.

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

    fn load_with(
        args: &[String],
        env: impl Fn(&str) -> Option<String>,
        default_path: &Path,
    ) -> Result<Self> {
        let mut options = Vec::new();
        let mut iter = args.iter();
        while let Some(arg) = iter.next() {
            let value = match arg.as_str() {
                "--open-registration" => "true",
                "--config" | "--bind" | "--http-bind" | "--db" | "--tls-cert" | "--tls-key"
                | "--tls-bind" | "--register-key" => iter
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
                for value in [&mut config.tls_cert, &mut config.tls_key]
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
        ] {
            if value.as_ref().is_some_and(|s| s.trim().is_empty()) {
                bail!("server setting {name} must not be empty");
            }
        }
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
                self.open_registration = match value.trim().to_ascii_lowercase().as_str() {
                    "1" | "true" | "yes" => true,
                    "0" | "false" | "no" | "" => false,
                    _ => bail!("open_registration must be true/false, yes/no, or 1/0"),
                };
            }
            _ => unreachable!(),
        }
        Ok(())
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
        Ok(())
    }
}
