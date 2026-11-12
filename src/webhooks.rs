use anyhow::{anyhow, Context, Result};
use hmac::{Hmac, Mac};
use rand::rngs::OsRng;
use rand::RngCore;
use reqwest::redirect::Policy;
use reqwest::Url;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use sha2::Sha256;
use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;
use uuid::Uuid;

use crate::history::unix_timestamp;

type HmacSha256 = Hmac<Sha256>;

const DEFAULT_EVENTS: &[&str] = &[
    "message", "mention", "dm", "report", "task", "git", "role", "lock",
];
const ALLOWED_NOTIFY_WHEN: &[&str] = &["always", "offline", "mentions"];
/// Mentions that address every member of the channel.
const GROUP_MENTIONS: &[&str] = &["here", "everyone", "all", "channel"];
const MAX_DELIVERY_ATTEMPTS: i64 = 5;

#[derive(Clone, Debug, Serialize)]
pub struct WebhookConfig {
    pub principal_id: String,
    pub url: String,
    #[serde(skip_serializing)]
    pub secret: String,
    pub events: Vec<String>,
    pub notify_when: String,
    pub enabled: bool,
    pub last_delivery_at: Option<i64>,
    pub last_status: Option<i64>,
    pub last_error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct DeliveryRecord {
    pub id: String,
    pub principal_id: String,
    pub event_type: String,
    pub channel: String,
    pub status: String,
    pub attempts: i64,
    pub http_status: Option<i64>,
    pub last_error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct WebhookEvent {
    pub event: String,
    pub channel: String,
    pub seq: Option<i64>,
    pub msg_id: Option<String>,
    pub from: String,
    pub to: Option<String>,
    pub message: String,
    pub created_at: i64,
    pub report: Option<serde_json::Value>,
    pub task: Option<serde_json::Value>,
    pub git: Option<serde_json::Value>,
    pub lock: Option<serde_json::Value>,
}

#[derive(Clone)]
pub struct WebhookStore {
    db: Arc<Mutex<Connection>>,
    notify: Arc<Notify>,
    http_client: reqwest::Client,
}

impl WebhookStore {
    pub fn new(db: Arc<Mutex<Connection>>) -> Self {
        let http_client = reqwest::Client::builder()
            .redirect(Policy::none())
            .timeout(Duration::from_secs(10))
            .build()
            .expect("build reqwest webhook client");

        Self {
            db,
            notify: Arc::new(Notify::new()),
            http_client,
        }
    }

    pub fn upsert(
        &self,
        principal_id: &str,
        raw_url: &str,
        events: Option<Vec<String>>,
        notify_when: Option<&str>,
        rotate_secret: bool,
    ) -> Result<WebhookConfig> {
        let url = validate_webhook_url(raw_url)?;
        let clean_events = normalize_events(events);
        let when = normalize_notify_when(notify_when)?;
        let events_json = serde_json::to_string(&clean_events)?;
        let now = unix_timestamp();

        let db = self.db.lock().expect("sqlite mutex poisoned");
        let existing_secret: Option<String> = db
            .query_row(
                "SELECT secret FROM webhooks WHERE principal_id = ?1",
                params![principal_id],
                |row| row.get(0),
            )
            .optional()
            .context("lookup existing webhook secret")?;

        let secret = match existing_secret {
            Some(saved) if !saved.is_empty() && !rotate_secret => saved,
            _ => generate_secret(),
        };

        db.execute(
            "INSERT INTO webhooks
             (principal_id, url, secret, events, notify_when, enabled, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?6)
             ON CONFLICT(principal_id) DO UPDATE SET
               url = excluded.url,
               secret = excluded.secret,
               events = excluded.events,
               notify_when = excluded.notify_when,
               enabled = 1,
               updated_at = excluded.updated_at",
            params![principal_id, url, secret, events_json, when, now],
        )
        .context("save webhook config")?;

        Self::get_locked(&db, principal_id)?.ok_or_else(|| anyhow!("failed to reload webhook"))
    }

    pub fn delete(&self, principal_id: &str) -> Result<bool> {
        let db = self.db.lock().expect("sqlite mutex poisoned");
        let removed = db
            .execute(
                "DELETE FROM webhooks WHERE principal_id = ?1",
                params![principal_id],
            )
            .context("delete webhook")?;
        Ok(removed > 0)
    }

    pub fn get(&self, principal_id: &str) -> Result<Option<WebhookConfig>> {
        let db = self.db.lock().expect("sqlite mutex poisoned");
        Self::get_locked(&db, principal_id)
    }

    pub fn configured_principals(&self) -> Result<HashSet<String>> {
        let db = self.db.lock().expect("sqlite mutex poisoned");
        let mut stmt = db.prepare("SELECT principal_id FROM webhooks WHERE enabled = 1")?;
        let ids = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<HashSet<_>, _>>()?;
        Ok(ids)
    }

    pub fn list_deliveries(&self, principal_id: &str, limit: i64) -> Result<Vec<DeliveryRecord>> {
        let capped = limit.clamp(1, 100);
        let db = self.db.lock().expect("sqlite mutex poisoned");
        let mut stmt = db
            .prepare(
                "SELECT id, principal_id, event_type, channel, status, attempts, http_status,
                        last_error, created_at, updated_at
                 FROM webhook_deliveries
                 WHERE principal_id = ?1
                 ORDER BY created_at DESC, id DESC
                 LIMIT ?2",
            )
            .context("prepare delivery list")?;
        let rows = stmt
            .query_map(params![principal_id, capped], Self::delivery_from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Match enabled webhook subscribers and enqueue durable deliveries for async push.
    pub fn enqueue_for_event(
        &self,
        event: &WebhookEvent,
        online_principals: &HashSet<String>,
        channel_members: &[String],
        role_mentions: &HashSet<String>,
    ) -> Result<usize> {
        let mentions = extract_mentions(&event.message);
        // `@here` and friends address everyone in the channel.
        let everyone = GROUP_MENTIONS.iter().any(|tag| mentions.contains(*tag));
        let member_set: HashSet<&str> = channel_members.iter().map(String::as_str).collect();

        let configs = {
            let db = self.db.lock().expect("sqlite mutex poisoned");
            Self::list_enabled_locked(&db)?
        };

        let mut queued = 0usize;
        let now = unix_timestamp();

        for cfg in configs {
            if cfg.principal_id.eq_ignore_ascii_case(&event.from) {
                continue;
            }

            let is_dm_recipient = event.event == "dm"
                && event
                    .to
                    .as_deref()
                    .is_some_and(|to| to.eq_ignore_ascii_case(&cfg.principal_id));
            if event.event == "dm" && !is_dm_recipient {
                continue;
            }

            let in_channel = member_set.contains(cfg.principal_id.as_str());
            let lowered_id = cfg.principal_id.to_ascii_lowercase();
            let is_mentioned = mentions.contains(&lowered_id)
                || role_mentions.contains(&lowered_id)
                || (everyone && in_channel);

            if !is_dm_recipient && !is_mentioned && !in_channel {
                continue;
            }

            match cfg.notify_when.as_str() {
                "offline" if online_principals.contains(&cfg.principal_id) => continue,
                "mentions" if !is_dm_recipient && !is_mentioned => continue,
                _ => {}
            }

            let wants_all = cfg.events.iter().any(|e| e == "all" || e == "*");
            let effective_event = if is_mentioned && !is_dm_recipient && event.event == "message" {
                "mention"
            } else {
                event.event.as_str()
            };

            let subscribed = wants_all
                || cfg.events.iter().any(|e| e == &event.event)
                || (is_mentioned && cfg.events.iter().any(|e| e == "mention"));

            if !subscribed {
                continue;
            }

            let delivery_id = format!("del_{}", Uuid::new_v4().simple());
            let mut per_recipient = event.clone();
            per_recipient.event = effective_event.to_string();
            if per_recipient.to.is_none() && is_mentioned {
                per_recipient.to = Some(cfg.principal_id.clone());
            }

            let payload = serde_json::json!({
                "delivery_id": delivery_id,
                "event": per_recipient.event,
                "channel": per_recipient.channel,
                "seq": per_recipient.seq,
                "msg_id": per_recipient.msg_id,
                "from": per_recipient.from,
                "to": per_recipient.to,
                "mentioned": is_mentioned,
                "message": per_recipient.message,
                "created_at": per_recipient.created_at,
                "report": per_recipient.report,
                "task": per_recipient.task,
                "git": per_recipient.git,
                "lock": per_recipient.lock,
            });
            let payload_json = serde_json::to_string(&payload)?;

            {
                let db = self.db.lock().expect("sqlite mutex poisoned");
                db.execute(
                    "INSERT INTO webhook_deliveries
                     (id, principal_id, event_type, channel, payload_json, status, attempts, next_attempt_at, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, 'pending', 0, ?6, ?6, ?6)",
                    params![
                        delivery_id,
                        cfg.principal_id,
                        effective_event,
                        event.channel,
                        payload_json,
                        now
                    ],
                )
                .context("enqueue webhook delivery")?;
            }
            queued += 1;
        }

        if queued > 0 {
            self.notify.notify_waiters();
        }
        Ok(queued)
    }

    pub async fn send_test_ping(&self, principal_id: &str, nick: &str) -> Result<DeliveryRecord> {
        let cfg = self.get(principal_id)?.ok_or_else(|| {
            anyhow!("No webhook URL configured for @{nick}; set one via POST /v1/webhooks first")
        })?;

        let delivery_id = format!("test_{}", Uuid::new_v4().simple());
        let now = unix_timestamp();
        let payload = serde_json::json!({
            "delivery_id": delivery_id,
            "event": "connection_test",
            "channel": "#general",
            "seq": null,
            "msg_id": null,
            "from": "aircd",
            "to": nick,
            "mentioned": true,
            "message": format!("Signed webhook connectivity test for @{nick}."),
            "created_at": now,
        });
        let payload_json = serde_json::to_string(&payload)?;

        {
            let db = self.db.lock().expect("sqlite mutex poisoned");
            db.execute(
                "INSERT INTO webhook_deliveries
                 (id, principal_id, event_type, channel, payload_json, status, attempts, next_attempt_at, created_at, updated_at)
                 VALUES (?1, ?2, 'connection_test', '#general', ?3, 'testing', 0, ?4, ?4, ?4)",
                params![delivery_id, principal_id, payload_json, now],
            )?;
        }

        match self
            .dispatch_http(
                &cfg.url,
                &cfg.secret,
                "connection_test",
                &delivery_id,
                &payload_json,
            )
            .await
        {
            Ok(status_code) => {
                self.mark_delivered(&delivery_id, principal_id, status_code)?;
                self.get_delivery(&delivery_id)?
                    .ok_or_else(|| anyhow!("missing test delivery row"))
            }
            Err((status_code, err_msg)) => {
                let db = self.db.lock().expect("sqlite mutex poisoned");
                let ts = unix_timestamp();
                db.execute(
                    "UPDATE webhook_deliveries
                     SET status = 'failed', attempts = 1, http_status = ?2, last_error = ?3, updated_at = ?4
                     WHERE id = ?1",
                    params![delivery_id, status_code, err_msg, ts],
                )?;
                db.execute(
                    "UPDATE webhooks SET last_delivery_at = ?2, last_status = ?3, last_error = ?4 WHERE principal_id = ?1",
                    params![principal_id, ts, status_code, err_msg],
                )?;
                Err(anyhow!("{err_msg}"))
            }
        }
    }

    pub fn spawn_worker(&self) -> tokio::task::JoinHandle<()> {
        let store = self.clone();
        tokio::spawn(async move {
            loop {
                if let Err(err) = store.process_due_deliveries().await {
                    eprintln!("webhooks worker error: {err:#}");
                }
                tokio::select! {
                    _ = store.notify.notified() => {}
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                }
            }
        })
    }

    async fn process_due_deliveries(&self) -> Result<()> {
        loop {
            let now = unix_timestamp();
            let batch: Vec<(String, String, String, String, String, i64, String, String)> = {
                let db = self.db.lock().expect("sqlite mutex poisoned");
                let mut stmt = db.prepare(
                    "SELECT d.id, d.principal_id, d.event_type, d.payload_json, w.url, d.attempts, w.secret, d.channel
                     FROM webhook_deliveries d
                     JOIN webhooks w ON w.principal_id = d.principal_id
                     WHERE d.status = 'pending' AND d.next_attempt_at <= ?1 AND w.enabled = 1
                     ORDER BY d.next_attempt_at ASC, d.created_at ASC
                     LIMIT 16",
                )?;
                let rows = stmt
                    .query_map(params![now], |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                            row.get(6)?,
                            row.get(7)?,
                        ))
                    })?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                rows
            };

            if batch.is_empty() {
                break;
            }

            for (
                delivery_id,
                principal_id,
                event_type,
                payload_json,
                url,
                attempts,
                secret,
                _channel,
            ) in batch
            {
                match self
                    .dispatch_http(&url, &secret, &event_type, &delivery_id, &payload_json)
                    .await
                {
                    Ok(status_code) => {
                        let _ = self.mark_delivered(&delivery_id, &principal_id, status_code);
                    }
                    Err((status_code, err_msg)) => {
                        let _ = self.mark_retry_or_failed(
                            &delivery_id,
                            &principal_id,
                            attempts + 1,
                            status_code,
                            &err_msg,
                        );
                    }
                }
            }
        }
        Ok(())
    }

    async fn dispatch_http(
        &self,
        url: &str,
        secret: &str,
        event_type: &str,
        delivery_id: &str,
        payload_json: &str,
    ) -> std::result::Result<i64, (Option<i64>, String)> {
        let signature = sign_payload(secret, payload_json.as_bytes());
        let response = self
            .http_client
            .post(url)
            .header("Content-Type", "application/json; charset=utf-8")
            .header("X-AirCD-Event", event_type)
            .header("X-AirCD-Delivery", delivery_id)
            .header("X-AirCD-Signature-256", &signature)
            .header("X-Agent-Ping-Event", event_type)
            .header("X-Agent-Ping-Delivery", delivery_id)
            .header("X-Agent-Ping-Signature-256", &signature)
            .body(payload_json.to_string())
            .send()
            .await
            .map_err(|err| (None, format!("request failed: {err}")))?;

        let status = response.status().as_u16() as i64;
        if (200..300).contains(&status) {
            Ok(status)
        } else {
            Err((Some(status), format!("webhook returned HTTP {status}")))
        }
    }

    fn mark_delivered(
        &self,
        delivery_id: &str,
        principal_id: &str,
        status_code: i64,
    ) -> Result<()> {
        let now = unix_timestamp();
        let db = self.db.lock().expect("sqlite mutex poisoned");
        db.execute(
            "UPDATE webhook_deliveries
             SET status = 'delivered', attempts = attempts + 1, http_status = ?2, last_error = NULL, updated_at = ?3
             WHERE id = ?1",
            params![delivery_id, status_code, now],
        )?;
        db.execute(
            "UPDATE webhooks
             SET last_delivery_at = ?2, last_status = ?3, last_error = NULL
             WHERE principal_id = ?1",
            params![principal_id, now, status_code],
        )?;
        Ok(())
    }

    fn mark_retry_or_failed(
        &self,
        delivery_id: &str,
        principal_id: &str,
        next_attempts: i64,
        status_code: Option<i64>,
        err_msg: &str,
    ) -> Result<()> {
        let now = unix_timestamp();
        let (next_status, next_attempt_at) = if next_attempts >= MAX_DELIVERY_ATTEMPTS {
            ("failed", now)
        } else {
            let backoff = 1i64.checked_shl(next_attempts.min(5) as u32).unwrap_or(32);
            ("pending", now + backoff)
        };

        let db = self.db.lock().expect("sqlite mutex poisoned");
        db.execute(
            "UPDATE webhook_deliveries
             SET status = ?2, attempts = ?3, next_attempt_at = ?4, http_status = ?5, last_error = ?6, updated_at = ?7
             WHERE id = ?1",
            params![
                delivery_id,
                next_status,
                next_attempts,
                next_attempt_at,
                status_code,
                err_msg,
                now
            ],
        )?;
        db.execute(
            "UPDATE webhooks
             SET last_delivery_at = ?2, last_status = ?3, last_error = ?4
             WHERE principal_id = ?1",
            params![principal_id, now, status_code, err_msg],
        )?;
        Ok(())
    }

    fn get_delivery(&self, id: &str) -> Result<Option<DeliveryRecord>> {
        let db = self.db.lock().expect("sqlite mutex poisoned");
        db.query_row(
            "SELECT id, principal_id, event_type, channel, status, attempts, http_status,
                    last_error, created_at, updated_at
             FROM webhook_deliveries
             WHERE id = ?1",
            params![id],
            Self::delivery_from_row,
        )
        .optional()
        .context("load delivery")
    }

    fn get_locked(db: &Connection, principal_id: &str) -> Result<Option<WebhookConfig>> {
        db.query_row(
            "SELECT principal_id, url, secret, events, notify_when, enabled,
                    last_delivery_at, last_status, last_error, created_at, updated_at
             FROM webhooks
             WHERE principal_id = ?1",
            params![principal_id],
            Self::from_row,
        )
        .optional()
        .context("load webhook")
    }

    fn list_enabled_locked(db: &Connection) -> Result<Vec<WebhookConfig>> {
        let mut stmt = db.prepare(
            "SELECT principal_id, url, secret, events, notify_when, enabled,
                    last_delivery_at, last_status, last_error, created_at, updated_at
             FROM webhooks
             WHERE enabled = 1",
        )?;
        let rows = stmt
            .query_map([], Self::from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<WebhookConfig> {
        let raw_events: String = row.get(3)?;
        let events: Vec<String> = serde_json::from_str(&raw_events)
            .unwrap_or_else(|_| DEFAULT_EVENTS.iter().map(|s| s.to_string()).collect());
        let enabled_int: i64 = row.get(5)?;
        Ok(WebhookConfig {
            principal_id: row.get(0)?,
            url: row.get(1)?,
            secret: row.get(2)?,
            events,
            notify_when: row.get(4)?,
            enabled: enabled_int != 0,
            last_delivery_at: row.get(6)?,
            last_status: row.get(7)?,
            last_error: row.get(8)?,
            created_at: row.get(9)?,
            updated_at: row.get(10)?,
        })
    }

    fn delivery_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<DeliveryRecord> {
        Ok(DeliveryRecord {
            id: row.get(0)?,
            principal_id: row.get(1)?,
            event_type: row.get(2)?,
            channel: row.get(3)?,
            status: row.get(4)?,
            attempts: row.get(5)?,
            http_status: row.get(6)?,
            last_error: row.get(7)?,
            created_at: row.get(8)?,
            updated_at: row.get(9)?,
        })
    }
}

pub fn sign_payload(secret: &str, raw_body: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC key valid");
    mac.update(raw_body);
    let bytes = mac.finalize().into_bytes();
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256={hex}")
}

pub fn extract_mentions(text: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'@' {
            let valid_boundary = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
            if valid_boundary {
                let start = i + 1;
                let mut end = start;
                while end < bytes.len()
                    && (bytes[end].is_ascii_alphanumeric()
                        || bytes[end] == b'_'
                        || bytes[end] == b'-')
                {
                    end += 1;
                }
                if end > start {
                    out.insert(text[start..end].to_ascii_lowercase());
                }
                i = end;
                continue;
            }
        }
        i += 1;
    }
    out
}

pub fn validate_webhook_url(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("webhook_url is required"));
    }
    let parsed = Url::parse(trimmed).context("Invalid webhook_url")?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(anyhow!("webhook_url must start with https:// or http://"));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(anyhow!("webhook_url must not contain credentials"));
    }
    if parsed.fragment().is_some() {
        return Err(anyhow!("webhook_url must not contain a #fragment"));
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow!("webhook_url is missing a host"))?;

    if parsed.scheme() == "http" && !is_private_or_local_host(host) {
        return Err(anyhow!(
            "Plain http:// webhooks are restricted to localhost, LAN, or Tailscale hosts; use https:// for public endpoints"
        ));
    }

    Ok(parsed.to_string())
}

fn is_private_or_local_host(host: &str) -> bool {
    let clean = host
        .trim_matches(|c| c == '[' || c == ']')
        .to_ascii_lowercase();
    if clean == "localhost"
        || clean == "localhost.localdomain"
        || clean == "host.docker.internal"
        || (!clean.contains('.') && clean.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
    {
        return true;
    }

    if let Ok(ip) = clean.parse::<IpAddr>() {
        return match ip {
            IpAddr::V4(v4) => is_local_v4(v4),
            IpAddr::V6(v6) => is_local_v6(v6),
        };
    }
    false
}

fn is_local_v4(v4: Ipv4Addr) -> bool {
    let o = v4.octets();
    v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || (o[0] == 100 && (64..=127).contains(&o[1]))
}

fn is_local_v6(v6: Ipv6Addr) -> bool {
    v6.is_loopback()
        || ((v6.segments()[0] & 0xfe00) == 0xfc00)
        || ((v6.segments()[0] & 0xffc0) == 0xfe80)
}

fn normalize_events(events: Option<Vec<String>>) -> Vec<String> {
    let Some(raw) = events else {
        return DEFAULT_EVENTS.iter().map(|s| s.to_string()).collect();
    };
    let mut out = Vec::new();
    for item in raw {
        let clean = item.trim().to_ascii_lowercase();
        if matches!(
            clean.as_str(),
            "message"
                | "mention"
                | "dm"
                | "report"
                | "task"
                | "git"
                | "role"
                | "lock"
                | "all"
                | "*"
        ) && !out.contains(&clean)
        {
            out.push(clean);
        }
    }
    if out.is_empty() {
        DEFAULT_EVENTS.iter().map(|s| s.to_string()).collect()
    } else {
        out
    }
}

fn normalize_notify_when(when: Option<&str>) -> Result<String> {
    let clean = when.unwrap_or("mentions").trim().to_ascii_lowercase();
    if ALLOWED_NOTIFY_WHEN.contains(&clean.as_str()) {
        Ok(clean)
    } else {
        Err(anyhow!(
            "notify_when must be one of: {}",
            ALLOWED_NOTIFY_WHEN.join(", ")
        ))
    }
}

fn generate_secret() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::{extract_mentions, sign_payload, validate_webhook_url};

    #[test]
    fn validates_local_and_lan_http_and_rejects_public_http() {
        assert!(validate_webhook_url("http://127.0.0.1:8787/hook").is_ok());
        assert!(validate_webhook_url("http://192.168.1.40:8080/hook").is_ok());
        assert!(validate_webhook_url("http://100.80.12.3:9000/hook").is_ok());
        assert!(validate_webhook_url("https://example.com/hook").is_ok());
        assert!(validate_webhook_url("http://example.com/hook").is_err());
    }

    #[test]
    fn extracts_mentions_without_matching_emails() {
        let found = extract_mentions("@Claude please check mailto user@example.com and @codex-2");
        assert!(found.contains("claude"));
        assert!(found.contains("codex-2"));
        assert!(!found.contains("example"));
    }

    #[test]
    fn signature_has_sha256_prefix() {
        let sig = sign_payload("top-secret", br#"{"hello":"world"}"#);
        assert!(sig.starts_with("sha256="));
        assert_eq!(sig.len(), 7 + 64);
    }
}
