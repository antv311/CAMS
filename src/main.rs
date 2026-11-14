mod alerts;
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
mod shutdown;
mod store;
mod task_metrics;
mod tasks;
mod tls;
mod token_usage;
mod tokens;
mod tui;
mod vacuum;
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
                       --db-journal-size-limit <bytes>  WAL journal size limit in bytes (env AIRCD_DB_JOURNAL_SIZE_LIMIT, default 67108864)\n  \
                       --no-db-vacuum           Disable automated incremental vacuum worker (env AIRCD_DB_VACUUM_ENABLED=0)\n  \
                       --db-vacuum-interval-secs <n>  Seconds between freelist checks (env AIRCD_DB_VACUUM_INTERVAL_SECS, default 300; 0 disables timer)\n  \
                       --db-vacuum-min-freelist-pages <n>  Minimum freelist pages to trigger vacuum (env AIRCD_DB_VACUUM_MIN_FREELIST_PAGES, default 64)\n  \
                       --db-vacuum-min-freelist-ratio <f>  Minimum freelist ratio 0.0..=1.0 (env AIRCD_DB_VACUUM_MIN_FREELIST_RATIO, default 0.0)\n  \
                       --db-vacuum-batch-pages <n>  Max pages reclaimed per non-blocking vacuum step (env AIRCD_DB_VACUUM_BATCH_PAGES, default 64)\n  \
                       --db-vacuum-step-delay-ms <n>  Delay between vacuum steps in milliseconds (env AIRCD_DB_VACUUM_STEP_DELAY_MS, default 25)\n  \
                       --db-vacuum-max-batches <n>  Max vacuum batches per scheduled pass (env AIRCD_DB_VACUUM_MAX_BATCHES, default 32; 0 unlimited)\n  \
                       --no-alerts              Disable Prometheus alert evaluator worker (env AIRCD_ALERTS_ENABLED=0)\n  \
                       --alerts-eval-interval-secs <n>  Seconds between alert evaluations (env AIRCD_ALERTS_EVAL_INTERVAL_SECS, default 15; 0 disables timer)\n  \
                       --alerts-webhook-url <url>  Optional direct webhook URL for alert notifications (env AIRCD_ALERTS_WEBHOOK_URL)\n  \
                       --alerts-webhook-secret <sec>  Optional HMAC-SHA256 secret for direct alert webhook (env AIRCD_ALERTS_WEBHOOK_SECRET)\n  \
                       --alerts-repeat-interval-secs <n>  Seconds between repeat firing notifications (env AIRCD_ALERTS_REPEAT_INTERVAL_SECS, default 3600)\n  \
                       --no-alerts-send-resolved  Suppress resolved webhook notifications (env AIRCD_ALERTS_SEND_RESOLVED=0)\n  \
                       --alerts-group-name <name>  Prometheus rule group name in /metrics/alerts/rules (env AIRCD_ALERTS_GROUP_NAME, default aircd_alerts)\n  \
                       --alerts-wake-failure-rate-threshold <f>  Wake failure ratio 0.0..=1.0 (default 0.25)\n  \
                       --alerts-wake-failure-window-secs <n>  Wake failure rolling window seconds (default 300)\n  \
                       --alerts-wake-failure-min-samples <n>  Min completed wakes before failure rate triggers (default 5)\n  \
                       --alerts-wake-failure-for-secs <n>  Sustained seconds before wake failure alert fires (default 120)\n  \
                       --alerts-queue-backlog-threshold <n>  Pending wake + webhook backlog threshold (default 50)\n  \
                       --alerts-queue-backlog-for-secs <n>  Sustained seconds before queue backlog alert fires (default 60)\n  \
                       --alerts-disk-used-ratio-threshold <f>  Disk used ratio 0.0..=1.0 (default 0.90; 0 disables)\n  \
                       --alerts-disk-min-free-bytes <n>  Minimum free bytes on DB volume (default 1073741824; 0 disables)\n  \
                       --alerts-disk-pressure-for-secs <n>  Sustained seconds before disk pressure alert fires (default 60)\n  \
                       --alerts-disk-path <path>  Override filesystem path checked for disk pressure\n  \
                       --drain-timeout-secs <n>  Max seconds to wait for active wake workers on shutdown (env AIRCD_DRAIN_TIMEOUT_SECS, default 30)\n  \
                       --http-drain-timeout-secs <n>  Max seconds for in-flight HTTP requests on shutdown (env AIRCD_HTTP_DRAIN_TIMEOUT_SECS, default 5)\n  \
                       --no-shutdown-checkpoint-leases  Disable checkpointing active task leases on shutdown (env AIRCD_SHUTDOWN_CHECKPOINT_LEASES=0)\n  \
                       --lease-checkpoint-extension-secs <n>  Lease extension seconds when checkpointing on shutdown (default 300)"
                );
        return Ok(());
    }
    let server_config = config::ServerConfig::load(&argv)?;
    let log_config = server_config.to_log_config()?;
    let backup_config = server_config.to_backup_config()?;
    let sqlite_config = server_config.to_sqlite_config()?;
    let vacuum_config = server_config.to_vacuum_config()?;
    let alert_config = server_config.to_alert_config()?;
    let shutdown_config = server_config.to_shutdown_config()?;
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
    .with_task_timeout_config(task_timeout_config)?
    .with_shutdown_config(shutdown_config.clone())?;
    server.vacuum().update_config(vacuum_config.clone());
    server.alerts().update_config(alert_config.clone());
    let stop_rx = server.shutdown().stop_accepting_rx();
    let webhook_worker = server.webhooks().spawn_worker_with_shutdown(stop_rx.clone());
    let backup_worker = server.backups().spawn_worker_with_shutdown(stop_rx.clone());
    let vacuum_worker = server.vacuum().spawn_worker_with_shutdown(stop_rx.clone());
    let alert_worker = server.spawn_alert_worker_with_shutdown(stop_rx.clone());

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
    let group_inactivity_worker = server.spawn_group_inactivity_worker_with_shutdown(stop_rx.clone());
    let task_timeout_worker = server.spawn_task_timeout_worker_with_shutdown(stop_rx.clone());
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
    if vacuum_config.enabled {
        println!(
            "  db vacuum:    incremental (interval={}s, min_pages={}, batch={}, delay={}ms)",
            vacuum_config.interval_secs,
            vacuum_config.min_freelist_pages,
            vacuum_config.batch_pages,
            vacuum_config.step_delay_ms,
        );
    } else {
        println!("  db vacuum:    disabled");
    }
    if alert_config.enabled {
        println!(
            "  alerts:       enabled (interval={}s, webhook={})",
            alert_config.eval_interval_secs,
            if alert_config.webhook_url.is_some() {
                "configured"
            } else {
                "none"
            }
        );
    } else {
        println!("  alerts:       disabled");
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
    println!(
        "  shutdown:     drain_timeout={}s, http_drain={}s, checkpoint_leases={}",
        shutdown_config.drain_timeout_secs,
        shutdown_config.http_drain_timeout_secs,
        shutdown_config.checkpoint_leases_on_drain,
    );
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
        let stop_rx = stop_rx.clone();
        async move {
            if tls_only {
                std::future::pending::<Result<()>>().await
            } else {
                server.listen_with_shutdown(&bind_addr, stop_rx).await
            }
        }
    };
    let plain_http_fut = {
        let server = server.clone();
        let http_bind = http_bind.clone();
        let runtime_info = runtime_info.clone();
        let stop_rx = stop_rx.clone();
        async move {
            if tls_only {
                std::future::pending::<Result<()>>().await
            } else {
                http::serve_with_shutdown(server, &http_bind, runtime_info, stop_rx).await
            }
        }
    };
    let irc_tls_fut = {
        let server = server.clone();
        let tls_bind = tls_bind.clone();
        let acceptor = tls_acceptor.clone();
        let stop_rx = stop_rx.clone();
        async move {
            match acceptor {
                Some(acceptor) => {
                    server
                        .listen_tls_with_shutdown(&tls_bind, acceptor, stop_rx)
                        .await
                }
                None => std::future::pending::<Result<()>>().await,
            }
        }
    };
    let https_fut = {
        let server = server.clone();
        let runtime_info = runtime_info;
        let stop_rx = stop_rx.clone();
        async move {
            match (http_tls_bind, tls_acceptor) {
                (Some(addr), Some(acceptor)) => {
                    http::serve_tls_with_shutdown(server, &addr, runtime_info, acceptor, stop_rx)
                        .await
                }
                _ => std::future::pending::<Result<()>>().await,
            }
        }
    };

    let mut listeners_handle = tokio::spawn(async move {
        tokio::try_join!(plain_irc_fut, plain_http_fut, irc_tls_fut, https_fut).map(|_| ())
    });
    let mut signal_listener = shutdown::ShutdownSignalListener::bind();

    let drain_signal = tokio::select! {
        res = &mut listeners_handle => {
            let run_result = res.map_err(|err| anyhow::anyhow!("aircd listener task panicked: {err}"))?;
            if let Err(err) = run_result {
                logger.log_error("server.startup_error", format!("aircd server error: {err:#}"));
                logger.flush();
                return Err(err);
            }
            return Ok(());
        }
        sig = signal_listener.recv() => sig.to_string(),
        req_sig = server.shutdown().wait_for_drain_request() => req_sig,
    };

    println!("\naircd received {drain_signal}; starting graceful worker drain...");
    let report = server
        .graceful_drain(&drain_signal, Some(&mut signal_listener))
        .await;
    println!(
        "aircd drain complete ({}ms): initial_active_wakes={}, detached_wakes={}, cancelled_pending={}, checkpointed_tasks={}, detached_irc={}",
        report.elapsed_ms,
        report.initial_active_wakes,
        report.detached_wake_runs,
        report.cancelled_pending_wakes,
        report.checkpointed_tasks,
        report.detached_irc_clients,
    );

    let http_drain_timeout = std::time::Duration::from_secs(shutdown_config.http_drain_timeout_secs);
    if !http_drain_timeout.is_zero() {
        match tokio::time::timeout(http_drain_timeout, &mut listeners_handle).await {
            Ok(Ok(Err(err))) => {
                logger.log_error(
                    "server.shutdown_error",
                    format!("listener error during shutdown: {err:#}"),
                );
            }
            Ok(Err(err)) => {
                logger.log_error(
                    "server.shutdown_error",
                    format!("listener task panicked during shutdown: {err}"),
                );
            }
            Ok(Ok(Ok(()))) => {}
            Err(_) => {
                listeners_handle.abort();
            }
        }
    } else {
        listeners_handle.abort();
    }

    let workers = vec![
        webhook_worker,
        backup_worker,
        vacuum_worker,
        alert_worker,
        group_inactivity_worker,
        task_timeout_worker,
    ];
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        for handle in workers {
            let _ = handle.await;
        }
    })
    .await;

    if let Err(err) = server.db().checkpoint(db::WalCheckpointMode::Truncate) {
        logger.log_error(
            "server.wal_checkpoint_error",
            format!("shutdown WAL checkpoint failed: {err:#}"),
        );
    }
    logger.log_security(
        logging::Severity::Info,
        "server.stopped",
        "success",
        Some("system"),
        None,
        None,
        format!("aircd stopped gracefully after {drain_signal}"),
    );
    logger.flush();
    drop(server);

    Ok(())
}
