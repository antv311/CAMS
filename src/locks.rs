use anyhow::{anyhow, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::fmt;
use std::sync::{Arc, Mutex};

use crate::history::unix_timestamp;

const DEFAULT_LOCK_TTL_SECONDS: i64 = 1800;
const MIN_LOCK_TTL_SECONDS: i64 = 30;
const MAX_LOCK_TTL_SECONDS: i64 = 86_400;

#[derive(Debug)]
pub struct LockConflictError {
    pub holder_nick: String,
    pub expires_at: i64,
    pub resource: String,
}

impl fmt::Display for LockConflictError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Resource '{}' is currently held by @{} (expires at {})",
            self.resource, self.holder_nick, self.expires_at
        )
    }
}

impl std::error::Error for LockConflictError {}

#[derive(Clone, Debug, Serialize)]
pub struct ResourceLock {
    pub resource: String,
    pub owner_id: String,
    pub owner_nick: String,
    pub channel: String,
    pub note: Option<String>,
    pub expires_at: i64,
    pub updated_at: i64,
}

#[derive(Clone)]
pub struct LockStore {
    db: Arc<Mutex<Connection>>,
}

impl LockStore {
    pub fn new(db: Arc<Mutex<Connection>>) -> Self {
        Self { db }
    }

    pub fn acquire(
        &self,
        raw_resource: &str,
        owner_id: &str,
        channel: &str,
        note: Option<&str>,
        ttl_seconds: Option<i64>,
    ) -> Result<ResourceLock> {
        let resource = sanitize_resource_name(raw_resource)?;
        let ttl = ttl_seconds
            .unwrap_or(DEFAULT_LOCK_TTL_SECONDS)
            .clamp(MIN_LOCK_TTL_SECONDS, MAX_LOCK_TTL_SECONDS);
        let clean_note = note
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.chars().take(240).collect::<String>());
        let now = unix_timestamp();
        let expires_at = now + ttl;

        let mut db = self.db.lock().expect("sqlite mutex poisoned");
        let tx = db.transaction().context("start lock acquire tx")?;

        if let Some(existing) = Self::get_locked(&tx, &resource)? {
            if existing.expires_at > now && !existing.owner_id.eq_ignore_ascii_case(owner_id) {
                return Err(anyhow::Error::new(LockConflictError {
                    holder_nick: existing.owner_nick,
                    expires_at: existing.expires_at,
                    resource: existing.resource,
                }));
            }
        }

        tx.execute(
            "INSERT INTO resource_locks (resource, owner_id, channel, note, expires_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(resource) DO UPDATE SET
               owner_id = excluded.owner_id,
               channel = excluded.channel,
               note = excluded.note,
               expires_at = excluded.expires_at,
               updated_at = excluded.updated_at",
            params![resource, owner_id, channel, clean_note, expires_at, now],
        )
        .context("upsert resource lock")?;

        tx.commit().context("commit resource lock")?;
        Self::get_locked(&db, &resource)?.ok_or_else(|| anyhow!("failed to reload resource lock"))
    }

    pub fn release(
        &self,
        raw_resource: &str,
        caller_id: &str,
        force: bool,
    ) -> Result<Option<ResourceLock>> {
        let resource = sanitize_resource_name(raw_resource)?;
        let now = unix_timestamp();
        let mut db = self.db.lock().expect("sqlite mutex poisoned");
        let tx = db.transaction().context("start lock release tx")?;

        let Some(existing) = Self::get_locked(&tx, &resource)? else {
            return Ok(None);
        };

        if existing.expires_at > now && !force && !existing.owner_id.eq_ignore_ascii_case(caller_id)
        {
            return Err(anyhow::Error::new(LockConflictError {
                holder_nick: existing.owner_nick,
                expires_at: existing.expires_at,
                resource: existing.resource,
            }));
        }

        tx.execute(
            "DELETE FROM resource_locks WHERE resource = ?1",
            params![resource],
        )
        .context("delete resource lock")?;
        tx.commit().context("commit resource lock release")?;
        Ok(Some(existing))
    }

    pub fn list(&self, channel: Option<&str>) -> Result<Vec<ResourceLock>> {
        let now = unix_timestamp();
        let db = self.db.lock().expect("sqlite mutex poisoned");
        let _ = db.execute(
            "DELETE FROM resource_locks WHERE expires_at <= ?1",
            params![now],
        );

        let mut stmt = db
            .prepare(
                "SELECT l.resource, l.owner_id, COALESCE(p.nick, l.owner_id), l.channel,
                        l.note, l.expires_at, l.updated_at
                 FROM resource_locks l
                 LEFT JOIN principals p ON p.id = l.owner_id
                 WHERE l.expires_at > ?1
                   AND (?2 IS NULL OR l.channel = ?2)
                 ORDER BY l.resource ASC",
            )
            .context("prepare lock list")?;

        let rows = stmt
            .query_map(params![now, channel], Self::from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn get_locked(conn: &Connection, resource: &str) -> Result<Option<ResourceLock>> {
        conn.query_row(
            "SELECT l.resource, l.owner_id, COALESCE(p.nick, l.owner_id), l.channel,
                    l.note, l.expires_at, l.updated_at
             FROM resource_locks l
             LEFT JOIN principals p ON p.id = l.owner_id
             WHERE l.resource = ?1",
            params![resource],
            Self::from_row,
        )
        .optional()
        .context("load resource lock")
    }

    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ResourceLock> {
        Ok(ResourceLock {
            resource: row.get(0)?,
            owner_id: row.get(1)?,
            owner_nick: row.get(2)?,
            channel: row.get(3)?,
            note: row.get(4)?,
            expires_at: row.get(5)?,
            updated_at: row.get(6)?,
        })
    }
}

pub fn sanitize_resource_name(raw: &str) -> Result<String> {
    let cleaned = raw.trim().to_ascii_lowercase();
    if cleaned.is_empty() || cleaned.len() > 64 {
        return Err(anyhow!("Resource name must be 1-64 characters"));
    }
    if !cleaned
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':' | '/'))
    {
        return Err(anyhow!(
            "Resource name may only contain letters, digits, '_', '-', '.', ':', and '/'"
        ));
    }
    Ok(cleaned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store;

    #[test]
    fn acquire_prevents_double_lock_and_allows_owner_release() -> Result<()> {
        let db = store::open(":memory:")?;
        let locks = LockStore::new(db);

        let first = locks.acquire(
            "quest-headset-1",
            "agent-a",
            "#general",
            Some("VR run"),
            Some(300),
        )?;
        assert_eq!(first.owner_id, "agent-a");

        let conflict = locks
            .acquire("quest-headset-1", "agent-b", "#general", None, Some(300))
            .expect_err("second agent must be blocked");
        assert!(conflict.downcast_ref::<LockConflictError>().is_some());

        // Non-owner cannot release without force.
        assert!(locks.release("quest-headset-1", "agent-b", false).is_err());

        // Owner release clears the lock.
        let released = locks
            .release("quest-headset-1", "agent-a", false)?
            .expect("lock existed");
        assert_eq!(released.resource, "quest-headset-1");
        assert!(locks.list(None)?.is_empty());

        // Now second agent can acquire it.
        let second = locks.acquire("quest-headset-1", "agent-b", "#general", None, Some(300))?;
        assert_eq!(second.owner_id, "agent-b");
        Ok(())
    }
}
