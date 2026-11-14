mod auth;
mod config;
mod gits;
mod history;
mod http;
mod locks;
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
                       --open-registration      Let anyone who can reach the server register (env AIRCD_OPEN_REGISTRATION=1)"
                );
        return Ok(());
    }
    let config::ServerConfig {
        bind: bind_addr,
        http_bind,
        db: db_path,
        tls_cert,
        tls_key,
        tls_bind,
        register_key,
        open_registration,
    } = config::ServerConfig::load(&argv)?;

    let (register_key, register_note) =
        resolve_register_key(register_key, open_registration, &db_path)?;
    let db = store::open(db_path)?;
    let server = Server::new(db);
    let _webhook_worker = server.webhooks().spawn_worker();

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
    println!("  registration: {register_note}");
    for host in &runtime_info.detected_hosts {
        println!(
            "  welcome/API: http://{}:{}   IRC: {}:{}",
            host, runtime_info.http_port, host, runtime_info.irc_port
        );
    }

    match (tls_cert, tls_key) {
        (Some(cert), Some(key)) => {
            let acceptor = load_tls_config(&cert, &key)?;
            let plain_server = server.clone();
            let http_server = server.clone();
            let tls_server = server;
            tokio::try_join!(
                plain_server.listen(&bind_addr),
                tls_server.listen_tls(&tls_bind, acceptor),
                http::serve(http_server, &http_bind, runtime_info),
            )?;
        }
        (Some(_), None) | (None, Some(_)) => {
            anyhow::bail!("both AIRCD_TLS_CERT and AIRCD_TLS_KEY must be set to enable TLS");
        }
        (None, None) => {
            let http_server = server.clone();
            tokio::try_join!(
                server.listen(&bind_addr),
                http::serve(http_server, &http_bind, runtime_info),
            )?;
        }
    }

    Ok(())
}
