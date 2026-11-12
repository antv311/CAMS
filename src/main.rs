mod auth;
mod gits;
mod history;
mod http;
mod locks;
mod protocol;
mod reports;
mod server;
mod store;
mod tasks;
mod tokens;
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

#[tokio::main]
async fn main() -> Result<()> {
    let mut bind_addr = std::env::var("AIRCD_BIND").unwrap_or_else(|_| "0.0.0.0:6667".to_string());
    let mut http_bind =
        std::env::var("AIRCD_HTTP_BIND").unwrap_or_else(|_| "0.0.0.0:6680".to_string());
    let mut db_path = std::env::var("AIRCD_DB").unwrap_or_else(|_| "aircd.sqlite3".to_string());
    let mut tls_cert = std::env::var("AIRCD_TLS_CERT").ok();
    let mut tls_key = std::env::var("AIRCD_TLS_KEY").ok();
    let mut tls_bind =
        std::env::var("AIRCD_TLS_BIND").unwrap_or_else(|_| "0.0.0.0:6697".to_string());

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bind" => {
                bind_addr = args.next().context("--bind requires host:port")?;
            }
            "--http-bind" => {
                http_bind = args.next().context("--http-bind requires host:port")?;
            }
            "--db" => {
                db_path = args.next().context("--db requires file path")?;
            }
            "--tls-cert" => {
                tls_cert = Some(args.next().context("--tls-cert requires path")?);
            }
            "--tls-key" => {
                tls_key = Some(args.next().context("--tls-key requires path")?);
            }
            "--tls-bind" => {
                tls_bind = args.next().context("--tls-bind requires host:port")?;
            }
            "-h" | "--help" => {
                println!(
                    "aircd - Lightweight IRC + Webhook + Roles + Equipment Locks + Git server\n\
                     Flags:\n  \
                       --bind <host:port>       IRC TCP listen address (env AIRCD_BIND, default 0.0.0.0:6667)\n  \
                       --http-bind <host:port>  HTTP welcome & REST API (env AIRCD_HTTP_BIND, default 0.0.0.0:6680)\n  \
                       --db <path>              SQLite database path (env AIRCD_DB, default aircd.sqlite3)\n  \
                       --tls-cert <path>        Optional PEM certificate for IRC TLS\n  \
                       --tls-key <path>         Optional PEM key for IRC TLS\n  \
                       --tls-bind <host:port>   IRC TLS listen address (default 0.0.0.0:6697)"
                );
                return Ok(());
            }
            other => {
                anyhow::bail!("unknown argument: {other} (use --help)");
            }
        }
    }

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
    let runtime_info = http::build_runtime_info(&bind_addr, &http_bind, tls_port);
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
