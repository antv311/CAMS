mod auth;
mod backup;
mod config;
mod db;
mod gits;
mod history;
mod http;
mod locks;
mod logging;
mod notes;
mod projects;
mod protocol;
mod reports;
mod rooms;
mod server;
mod store;
mod tasks;
mod tokens;
mod tui;
mod wake;
mod webhooks;

use anyhow::{Context, Result};
use server::Server;
use std::sync::Arc;

fn load_tls_config(cert_path: &str, key_path: &str) -> Result<tokio_rustls::TlsAcceptor> {
    let cert_data =
        std::fs::read(cert_path).with_context(|| format!("read TLS cert: {cert_path}"))?;
    let key_data = std::fs::read(key_path).with_context(|| format!("read TLS key: {key_path}"))?;

    let certs: Vec<_> = rustls_pemfile::certs(&mut cert_data.as_slice())
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("parse TLS certificates")?;
    if certs.is_empty() {
        anyhow::bail!("no certificates found in {cert_path}");
    }

    let key = rustls_pemfile::private_key(&mut key_data.as_slice())
        .context("parse TLS private key")?
        .ok_or_else(|| anyhow::anyhow!("no private key found in {key_path}"))?;

    let config = tokio_rustls::rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("build TLS server config")?;

    Ok(tokio_rustls::TlsAcceptor::from(Arc::new(config)))
}

/// Decide which invite key guards `POST /v1/register`. The key itself is
/// never printed; only where it came from.
fn resolve_register_key(
    explicit: Option<String>,
    open_registration: bool,
    db_path: &str,
) -> Result<(Option<String>, String)> {
    if open_registration {
        return Ok((
            None,
            "OPEN - anyone who can reach this server can register".to_string(),
        ));
    }
    if let Some(key) = explicit {
        return Ok((
            Some(key.trim().to_string()),
            "invite key required (from flag/env/config)".to_string(),
        ));
    }
    if db_path == ":memory:" {
        return Ok((None, "OPEN - in-memory database".to_string()));
    }

    let key_path = std::path::Path::new(db_path)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("aircd-owner.key");
    if let Ok(existing) = std::fs::read_to_string(&key_path) {
        let existing = existing.trim().to_string();
        if !existing.is_empty() {
            return Ok((
                Some(existing),
                format!("invite key required (stored in {})", key_path.display()),
            ));
        }
    }

    use rand::RngCore;
    let mut raw = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut raw);
    let key: String = raw.iter().map(|b| format!("{b:02x}")).collect();
    std::fs::write(&key_path, format!("{key}\n"))
        .with_context(|| format!("write owner invite key: {}", key_path.display()))?;
    Ok((
        Some(key),
        format!(
            "invite key required (new key written to {})",
            key_path.display()
        ),
    ))
}

#[tokio::main]
async fn main() -> Result<()> {
    // `aircd tui` attaches a dashboard to a hub that is already running.
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.first().map(String::as_str) == Some("tui") {
        return tui::run(argv[1..].to_vec()).await;
    }

    if argv
        .iter()
        .any(|arg| matches!(arg.as_str(), "-h" | "--help"))
    {
        println!(
                    "aircd - Lightweight IRC + Webhook + Roles + Equipment Locks + Git server\n\
                     Run `aircd tui` to open the dashboard and chat for a running hub.\n\
                     Settings: flags > environment > aircd.toml > defaults.\n\
                     Config: AIRCD_CONFIG or AIRCD_CONFIG_DIR/aircd.toml (see aircd.toml.example).\n\
                     Flags:\n  \
                       --config <path>         Editable server TOML (env AIRCD_CONFIG)\n  \
                       --bind <host:port>       IRC TCP listen address (env AIRCD_BIND, default 127.0.0.1:6667)\n  \
                       --http-bind <host:port>  HTTP welcome & REST API (env AIRCD_HTTP_BIND, default 127.0.0.1:6680)\n  \
                       --db <path>              SQLite database path (env AIRCD_DB, default aircd.sqlite3)\n  \
                       --tls-cert <path>        Optional PEM certificate for IRC TLS\n  \
                       --tls-key <path>         Optional PEM key for IRC TLS\n  \
                       --tls-bind <host:port>   IRC TLS listen address (default 127.0.0.1:6697)\n  \
                       --register-key <key>     Invite key required to register (env AIRCD_REGISTER_KEY;\n  \
                                                default: generated once in aircd-owner.key beside the DB)\n  \
                       --open-registration      Let anyone who can reach the server register (env AIRCD_OPEN_REGISTRATION=1)\n  \
                       --windows-event-log      Forward server errors and security audits to Windows Application Event Log (env AIRCD_WINDOWS_EVENT_LOG=1)\n  \
                       --windows-event-log-source <name>  Windows Event Log source name (default aircd)\n  \
                       --syslog-target <target> Forward server errors and security audits to syslog (udp://host:port, tcp://host:port, unix:///path)\n  \
                       --syslog-format <fmt>    Syslog message format: rfc5424 (default) or rfc3164\n  \
                       --syslog-facility <fac>  Syslog facility: auto (default), daemon, auth, authpriv, user, syslog, or local0..local7\n  \
                       --backup-dir <dir>       Snapshot backup directory (env AIRCD_BACKUP_DIR, default <db_dir>/backups)\n  \
                       --backup-interval-secs <n>  Seconds between scheduled backups (env AIRCD_BACKUP_INTERVAL_SECS, default 3600; 0 disables timer)\n  \
                       --backup-retention <n>   Number of snapshots to retain (env AIRCD_BACKUP_RETENTION, default 10)\n  \
                       --backup-on-startup      Capture a snapshot immediately on startup (env AIRCD_BACKUP_ON_STARTUP=1)\n  \
                       --no-backups             Disable automated snapshot backup worker (env AIRCD_BACKUP_ENABLED=0)\n  \
                       --db-pool-size <n>       Read-only SQLite connection pool size (env AIRCD_DB_POOL_SIZE, default 8)\n  \
                       --db-busy-timeout-ms <n> SQLite busy_timeout in milliseconds (env AIRCD_DB_BUSY_TIMEOUT_MS, default 5000)\n  \
                       --db-synchronous <mode>  SQLite WAL synchronous mode: off, normal, full, extra (env AIRCD_DB_SYNCHRONOUS, default normal)\n  \
                       --db-wal-autocheckpoint <pages>  WAL auto-checkpoint threshold in pages (env AIRCD_DB_WAL_AUTOCHECKPOINT, default 1000)\n  \
                       --db-cache-size-kib <n>  Per-connection SQLite cache size in KiB (env AIRCD_DB_CACHE_SIZE_KIB, default 16384)\n  \
                       --db-journal-size-limit <bytes>  WAL journal size limit in bytes (env AIRCD_DB_JOURNAL_SIZE_LIMIT, default 67108864)"
                );
        return Ok(());
    }
    let server_config = config::ServerConfig::load(&argv)?;
    let log_config = server_config.to_log_config()?;
    let backup_config = server_config.to_backup_config()?;
    let sqlite_config = server_config.to_sqlite_config()?;
    let logger = logging::EventForwarder::new(log_config)?;
    let config::ServerConfig {
        bind: bind_addr,
        http_bind,
        db: db_path,
        tls_cert,
        tls_key,
        tls_bind,
        register_key,
        open_registration,
        ..
    } = server_config;

    let (register_key, register_note) =
        resolve_register_key(register_key, open_registration, &db_path)?;
    let db = store::open_with_config(&db_path, sqlite_config.clone())?;
    let server = Server::with_logger_and_backup(db, logger.clone(), backup_config.clone());
    let _webhook_worker = server.webhooks().spawn_worker();
    let _backup_worker = server.backups().spawn_worker();

    let tls_port = if tls_cert.is_some() && tls_key.is_some() {
        tls_bind
            .rsplit_once(':')
            .and_then(|(_, p)| p.parse::<u16>().ok())
    } else {
        None
    };
    let mut runtime_info = http::build_runtime_info(&bind_addr, &http_bind, tls_port);
    runtime_info.register_key = register_key;
    let wake_host = http_bind
        .rsplit_once(':')
        .map(|(host, _)| host)
        .filter(|host| !matches!(*host, "0.0.0.0" | "[::]" | "::"))
        .unwrap_or("127.0.0.1");
    server
        .wakes()
        .set_server_url(&format!("http://{wake_host}:{}", runtime_info.http_port));
    let _group_inactivity_worker = server.spawn_group_inactivity_worker();
    println!("  registration: {register_note}");
    println!(
        "  database:     {} (pool={}, busy={}ms, sync={}, autocheckpoint={}p)",
        db_path,
        if db_path == ":memory:" {
            1
        } else {
            sqlite_config.read_pool_size
        },
        sqlite_config.busy_timeout_ms,
        sqlite_config.synchronous,
        sqlite_config.wal_autocheckpoint,
    );
    if backup_config.enabled {
        if let Some(dir) = backup_config.effective_backup_dir() {
            println!(
                "  backups:      {} (interval={}s, retention={})",
                dir.display(),
                backup_config.interval_secs,
                backup_config.retention
            );
        }
    }
    for host in &runtime_info.detected_hosts {
        println!(
            "  welcome/API: http://{}:{}   IRC: {}:{}",
            host, runtime_info.http_port, host, runtime_info.irc_port
        );
    }

    logger.log_security(
        logging::Severity::Info,
        "server.started",
        "success",
        Some("system"),
        None,
        None,
        format!(
            "aircd started (irc={bind_addr}, http={http_bind}, tls={})",
            tls_cert.is_some() && tls_key.is_some()
        ),
    );

    let run_result = match (tls_cert, tls_key) {
        (Some(cert), Some(key)) => {
            let acceptor = load_tls_config(&cert, &key)?;
            let plain_server = server.clone();
            let http_server = server.clone();
            let tls_server = server;
            tokio::try_join!(
                plain_server.listen(&bind_addr),
                tls_server.listen_tls(&tls_bind, acceptor),
                http::serve(http_server, &http_bind, runtime_info),
            )
            .map(|_| ())
        }
        (Some(_), None) | (None, Some(_)) => {
            Err(anyhow::anyhow!(
                "both AIRCD_TLS_CERT and AIRCD_TLS_KEY must be set to enable TLS"
            ))
        }
        (None, None) => {
            let http_server = server.clone();
            tokio::try_join!(
                server.listen(&bind_addr),
                http::serve(http_server, &http_bind, runtime_info),
            )
            .map(|_| ())
        }
    };

    if let Err(err) = run_result {
        logger.log_error("server.startup_error", format!("aircd server error: {err:#}"));
        logger.flush();
        return Err(err);
    }

    Ok(())
}
