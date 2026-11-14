use anyhow::{anyhow, Context, Result};
use hmac::{Hmac, Mac};
use rand::rngs::OsRng;
use rand::RngCore;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use sha2::Sha256;

use crate::history::unix_timestamp;
use crate::store::SharedConnection;

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone, Debug, Serialize)]
pub struct GitRepo {
    pub name: String,
    pub channel: String,
    pub local_path: String,
    pub remote_url: Option<String>,
    pub default_branch: Option<String>,
    pub webhook_configured: bool,
    pub last_commit: Option<String>,
    pub last_branch: Option<String>,
    pub last_event_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone)]
pub struct GitRepoStore {
    db: SharedConnection,
}

impl GitRepoStore {
    pub fn new(db: SharedConnection) -> Self {
        Self { db }
    }

    pub fn upsert(
        &self,
        name: &str,
        channel: &str,
        local_path: &str,
        remote_url: Option<&str>,
        default_branch: Option<&str>,
        webhook_secret: Option<&str>,
    ) -> Result<(GitRepo, Option<String>)> {
        let clean_name = sanitize_repo_name(name)?;
        let clean_path = local_path.trim();
        if clean_path.is_empty() {
            return Err(anyhow!("local_path is required"));
        }

        let now = unix_timestamp();
        let db = self.db.write()?;
        let existing_secret: Option<String> = db
            .query_row(
                "SELECT webhook_secret FROM git_repos WHERE name = ?1",
                params![clean_name],
                |row| row.get(0),
            )
            .optional()
            .context("check existing git repo")?
            .flatten();

        let (secret_to_save, newly_issued_secret) =
            match webhook_secret.map(str::trim).filter(|s| !s.is_empty()) {
                Some(explicit) => (explicit.to_string(), None),
                None => match existing_secret {
                    Some(saved) if !saved.is_empty() => (saved, None),
                    _ => {
                        let generated = random_hex_secret();
                        (generated.clone(), Some(generated))
                    }
                },
            };

        db.execute(
            "INSERT INTO git_repos
             (name, channel, local_path, remote_url, default_branch, webhook_secret, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)
             ON CONFLICT(name) DO UPDATE SET
               channel = excluded.channel,
               local_path = excluded.local_path,
               remote_url = COALESCE(excluded.remote_url, git_repos.remote_url),
               default_branch = COALESCE(excluded.default_branch, git_repos.default_branch),
               webhook_secret = CASE
                 WHEN ?6 != '' THEN ?6
                 ELSE git_repos.webhook_secret
               END,
               updated_at = excluded.updated_at",
            params![
                clean_name,
                channel,
                clean_path,
                remote_url.map(str::trim).filter(|s| !s.is_empty()),
                default_branch.map(str::trim).filter(|s| !s.is_empty()),
                secret_to_save,
                now,
            ],
        )
        .context("upsert git repo")?;

        let repo = Self::get_locked(&db, &clean_name)?
            .ok_or_else(|| anyhow!("failed to reload registered git repo"))?;
        Ok((repo, newly_issued_secret))
    }

    pub fn get_with_secret(&self, name: &str) -> Result<Option<(GitRepo, Option<String>)>> {
        let db = self.db.read()?;
        db.query_row(
            "SELECT name, channel, local_path, remote_url, default_branch, webhook_secret,
                    last_commit, last_branch, last_event_at, created_at, updated_at
             FROM git_repos
             WHERE LOWER(name) = LOWER(?1)",
            params![name.trim()],
            |row| {
                let secret: Option<String> = row.get(5)?;
                let repo = GitRepo {
                    name: row.get(0)?,
                    channel: row.get(1)?,
                    local_path: row.get(2)?,
                    remote_url: row.get(3)?,
                    default_branch: row.get(4)?,
                    webhook_configured: secret.as_deref().is_some_and(|s| !s.is_empty()),
                    last_commit: row.get(6)?,
                    last_branch: row.get(7)?,
                    last_event_at: row.get(8)?,
                    created_at: row.get(9)?,
                    updated_at: row.get(10)?,
                };
                Ok((repo, secret))
            },
        )
        .optional()
        .context("load git repo")
    }

    pub fn list(&self) -> Result<Vec<GitRepo>> {
        let db = self.db.read()?;
        let mut stmt = db
            .prepare(
                "SELECT name, channel, local_path, remote_url, default_branch, webhook_secret,
                        last_commit, last_branch, last_event_at, created_at, updated_at
                 FROM git_repos
                 ORDER BY name ASC",
            )
            .context("prepare git_repos list")?;

        let rows = stmt
            .query_map([], Self::from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn record_event(
        &self,
        name: &str,
        branch: Option<&str>,
        commit: Option<&str>,
    ) -> Result<()> {
        let now = unix_timestamp();
        let db = self.db.write()?;
        db.execute(
            "UPDATE git_repos
             SET last_branch = COALESCE(?2, last_branch),
                 last_commit = COALESCE(?3, last_commit),
                 last_event_at = ?4,
                 updated_at = ?4
             WHERE LOWER(name) = LOWER(?1)",
            params![
                name.trim(),
                branch.map(str::trim).filter(|s| !s.is_empty()),
                commit.map(str::trim).filter(|s| !s.is_empty()),
                now,
            ],
        )
        .context("record git event")?;
        Ok(())
    }

    fn get_locked(db: &Connection, name: &str) -> Result<Option<GitRepo>> {
        db.query_row(
            "SELECT name, channel, local_path, remote_url, default_branch, webhook_secret,
                    last_commit, last_branch, last_event_at, created_at, updated_at
             FROM git_repos
             WHERE name = ?1",
            params![name],
            Self::from_row,
        )
        .optional()
        .context("load git repo")
    }

    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<GitRepo> {
        let secret: Option<String> = row.get(5)?;
        Ok(GitRepo {
            name: row.get(0)?,
            channel: row.get(1)?,
            local_path: row.get(2)?,
            remote_url: row.get(3)?,
            default_branch: row.get(4)?,
            webhook_configured: secret.as_deref().is_some_and(|s| !s.is_empty()),
            last_commit: row.get(6)?,
            last_branch: row.get(7)?,
            last_event_at: row.get(8)?,
            created_at: row.get(9)?,
            updated_at: row.get(10)?,
        })
    }
}

pub fn sanitize_repo_name(raw: &str) -> Result<String> {
    let cleaned = raw
        .trim()
        .trim_start_matches('#')
        .to_ascii_lowercase()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string();

    if cleaned.is_empty() || cleaned.len() > 64 {
        return Err(anyhow!("Repo name must be 1-64 identifier characters"));
    }
    Ok(cleaned)
}

pub fn verify_github_signature(secret: &str, raw_body: &[u8], signature_header: &str) -> bool {
    let Some(hex_digest) = signature_header.strip_prefix("sha256=") else {
        return false;
    };
    let Ok(sig_bytes) = hex_decode(hex_digest.trim()) else {
        return false;
    };
    let Ok(mut mac) = HmacSha256::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(raw_body);
    mac.verify_slice(&sig_bytes).is_ok()
}

fn random_hex_secret() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_decode(input: &str) -> Result<Vec<u8>> {
    if input.len() % 2 != 0 {
        return Err(anyhow!("odd hex length"));
    }
    let mut out = Vec::with_capacity(input.len() / 2);
    for pair in input.as_bytes().chunks_exact(2) {
        let s = std::str::from_utf8(pair)?;
        out.push(u8::from_str_radix(s, 16)?);
    }
    Ok(out)
}
