mod attachments;
mod auth;
mod backup;
mod config;
mod dashboard;
mod db;
mod gits;
mod history;
mod http;
mod locks;
mod logging;
mod metrics;
mod notes;
mod projects;
mod protocol;
mod rate_limit;
mod reports;
mod rooms;
mod server;
mod store;
mod task_metrics;
mod tasks;
mod tls;
mod tokens;
mod tui;
mod wake;
mod webhooks;

use anyhow::{Context, Result};
use server::Server;

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
                       --tls-cert <path>        Optional PEM certificate for IRC/HTTP TLS\n  \
                       --tls-key <path>         Optional PEM key for IRC/HTTP TLS\n  \
                       --tls-bind <host:port>   IRC TLS listen address (default 127.0.0.1:6697)\n  \
                       --tls-client-ca <path>   Optional PEM CA bundle for client certificate verification (mTLS)\n  \
                       --tls-client-auth <mode> mTLS mode: none, optional, or required (default: required if --tls-client-ca is set)\n  \
                       --http-tls-bind <addr>   Optional HTTPS welcome & API listen address (uses TLS/mTLS config)\n  \
                       --tls-only               Disable plaintext IRC and HTTP listeners (env AIRCD_TLS_ONLY=1)\n  \
                       --register-key <key>     Invite key required to register (env AIRCD_REGISTER_KEY;\n  \
                                                default: generated once in aircd-owner.key beside the DB)\n  \
                       --open-registration      Let anyone who can reach the server register (env AIRCD_OPEN_REGISTRATION=1)\n  \
                       --token-ttl-seconds <n>  Default bearer token lifetime in seconds (env AIRCD_TOKEN_TTL_SECONDS; default: non-expiring)\n  \
                       --no-rate-limit          Disable HTTP token-bucket rate limiting (env AIRCD_RATE_LIMIT_ENABLED=0)\n  \
                       --rate-limit-ip-rps <n>  HTTP token-bucket refill rate per client IP (env AIRCD_RATE_LIMIT_IP_RPS, default 100; 0 disables)\n  \
                       --rate-limit-ip-burst <n>  HTTP token-bucket burst capacity per client IP (env AIRCD_RATE_LIMIT_IP_BURST, default 200)\n  \
                       --rate-limit-principal-rps <n>  HTTP token-bucket refill rate per principal (env AIRCD_RATE_LIMIT_PRINCIPAL_RPS, default 30; 0 disables)\n  \
                       --rate-limit-principal-burst <n>  HTTP token-bucket burst capacity per principal (env AIRCD_RATE_LIMIT_PRINCIPAL_BURST, default 60)\n  \
                       --rate-limit-trust-proxy-headers  Trust X-Forwarded-For / X-Real-IP headers for rate-limit IP (env AIRCD_RATE_LIMIT_TRUST_PROXY_HEADERS=1)\n  \
                       --rate-limit-max-buckets <n>  Max tracked IP/principal rate-limit buckets (env AIRCD_RATE_LIMIT_MAX_BUCKETS, default 10000)\n  \
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
    let tls_spec = server_config.to_tls_config()?;
    let logger = logging::EventForwarder::new(log_config)?;
    let task_timeout_config = server_config.task_timeout.clone();
    task_timeout_config.validate()?;
    let config::ServerConfig {
        bind: bind_addr,
        http_bind,
        db: db_path,
        tls_bind,
        http_tls_bind,
        tls_only,
        register_key,
        open_registration,
        auth: auth_config,
        rate_limit: rate_limit_config,
        ..
    } = server_config;

    let (register_key, register_note) =
        resolve_register_key(register_key, open_registration, &db_path)?;
    let db = store::open_with_config(&db_path, sqlite_config.clone())?;
    let server = Server::with_logger_backup_and_ttl(
        db,
        logger.clone(),
        backup_config.clone(),
        auth_config.token_ttl_seconds,
    )
    .with_task_timeout_config(task_timeout_config)?;
    let _webhook_worker = server.webhooks().spawn_worker();
    let _backup_worker = server.backups().spawn_worker();

    let tls_acceptor = tls_spec
        .as_ref()
        .map(tls::build_tls_acceptor)
        .transpose()?;
    let tls_client_auth_mode = tls_spec
        .as_ref()
        .map(|spec| spec.client_auth.as_str().to_string())
        .unwrap_or_else(|| "none".to_string());

    let tls_port = if tls_acceptor.is_some() {
        tls_bind
            .rsplit_once(':')
            .and_then(|(_, p)| p.parse::<u16>().ok())
    } else {
        None
    };
    let http_tls_port = http_tls_bind
        .as_deref()
        .and_then(|addr| addr.rsplit_once(':'))
        .and_then(|(_, p)| p.parse::<u16>().ok());
    let mut runtime_info = http::build_runtime_info(&bind_addr, &http_bind, tls_port);
    runtime_info.http_tls_port = http_tls_port;
    runtime_info.tls_client_auth = tls_client_auth_mode.clone();
    runtime_info.tls_only = tls_only;
    runtime_info.register_key = register_key;
    runtime_info.rate_limit = rate_limit_config.clone();
    let wake_host = if tls_only {
        http_tls_bind.as_deref().unwrap_or(&http_bind)
    } else {
        &http_bind
    }
    .rsplit_once(':')
    .map(|(host, _)| host)
    .filter(|host| !matches!(*host, "0.0.0.0" | "[::]" | "::"))
    .unwrap_or("127.0.0.1");
    let wake_url = if tls_only {
        format!(
            "https://{wake_host}:{}",
            http_tls_port.unwrap_or(runtime_info.http_port)
        )
    } else {
        format!("http://{wake_host}:{}", runtime_info.http_port)
    };
    server.wakes().set_server_url(&wake_url);
    let _group_inactivity_worker = server.spawn_group_inactivity_worker();
    let _task_timeout_worker = server.spawn_task_timeout_worker();
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
    if rate_limit_config.enabled {
        println!(
            "  rate limit:   ip={}/s (burst={}), principal={}/s (burst={})",
            rate_limit_config.ip_requests_per_sec,
            rate_limit_config.ip_burst,
            rate_limit_config.principal_requests_per_sec,
            rate_limit_config.principal_burst,
        );
    } else {
        println!("  rate limit:   disabled");
    }
    if tls_acceptor.is_some() {
        println!(
            "  TLS:          irc={tls_bind}, https={}, client_auth={tls_client_auth_mode}, tls_only={tls_only}",
            http_tls_bind.as_deref().unwrap_or("disabled")
        );
    }
    if !tls_only {
        for host in &runtime_info.detected_hosts {
            println!(
                "  welcome/API: http://{}:{}   IRC: {}:{}",
                host, runtime_info.http_port, host, runtime_info.irc_port
            );
        }
    }

    logger.log_security(
        logging::Severity::Info,
        "server.started",
        "success",
        Some("system"),
        None,
        None,
        format!(
            "aircd started (irc={bind_addr}, http={http_bind}, tls={}, https={}, client_auth={tls_client_auth_mode}, tls_only={tls_only})",
            tls_acceptor.is_some(),
            http_tls_bind.as_deref().unwrap_or("none"),
        ),
    );

    let plain_irc_fut = {
        let server = server.clone();
        let bind_addr = bind_addr.clone();
        async move {
            if tls_only {
                std::future::pending::<Result<()>>().await
            } else {
                server.listen(&bind_addr).await
            }
        }
    };
    let plain_http_fut = {
        let server = server.clone();
        let http_bind = http_bind.clone();
        let runtime_info = runtime_info.clone();
        async move {
            if tls_only {
                std::future::pending::<Result<()>>().await
            } else {
                http::serve(server, &http_bind, runtime_info).await
            }
        }
    };
    let irc_tls_fut = {
        let server = server.clone();
        let tls_bind = tls_bind.clone();
        let acceptor = tls_acceptor.clone();
        async move {
            match acceptor {
                Some(acceptor) => server.listen_tls(&tls_bind, acceptor).await,
                None => std::future::pending::<Result<()>>().await,
            }
        }
    };
    let https_fut = {
        let server = server;
        let runtime_info = runtime_info;
        async move {
            match (http_tls_bind, tls_acceptor) {
                (Some(addr), Some(acceptor)) => {
                    http::serve_tls(server, &addr, runtime_info, acceptor).await
                }
                _ => std::future::pending::<Result<()>>().await,
            }
        }
    };

    let run_result =
        tokio::try_join!(plain_irc_fut, plain_http_fut, irc_tls_fut, https_fut).map(|_| ());

    if let Err(err) = run_result {
        logger.log_error("server.startup_error", format!("aircd server error: {err:#}"));
        logger.flush();
        return Err(err);
    }

    Ok(())
}
