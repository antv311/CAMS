use anyhow::{anyhow, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::tokens;

#[derive(Debug)]
pub struct DuplicatePrincipalError(pub String);

impl fmt::Display for DuplicatePrincipalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DuplicatePrincipalError {}

#[derive(Clone, Debug, Serialize)]
pub struct Principal {
    pub id: String,
    pub nick: String,
    pub team: Option<String>,
    pub role: String,
}

#[derive(Clone)]
pub struct AuthStore {
    db: Arc<Mutex<Connection>>,
    legacy_hits: Arc<AtomicU64>,
}

impl AuthStore {
    pub fn new(db: Arc<Mutex<Connection>>) -> Self {
        Self {
            db,
            legacy_hits: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Authenticate an incoming token. Tries the new `principal_tokens` path
    /// first when the token has the `airc_` prefix, falls back to the legacy
    /// `principals.token` column so existing deployments keep working during
    /// the migration window. PR C will drop the legacy column once all clients
    /// have been rotated.
    pub fn authenticate(&self, token: &str) -> Result<Principal> {
        if let Some((token_id, secret)) = tokens::parse(token) {
            if let Some(principal) = self.authenticate_new(&token_id, &secret)? {
                return Ok(principal);
            }
        }

        let principal = self.authenticate_legacy(token)?;
        let total = self.legacy_hits.fetch_add(1, Ordering::Relaxed) + 1;
        eprintln!(
            "auth: legacy plaintext token accepted for principal {} (legacy_hits={})",
            principal.nick, total,
        );
        Ok(principal)
    }

    /// Total number of successful legacy-path authentications since startup.
    /// Used to gauge when the legacy column in `principals` can be dropped
    /// (PR C). Exposed for future CLI/status endpoints.
    #[allow(dead_code)]
    pub fn legacy_hit_count(&self) -> u64 {
        self.legacy_hits.load(Ordering::Relaxed)
    }

    fn authenticate_new(&self, token_id: &str, secret: &str) -> Result<Option<Principal>> {
        let db = self.db.lock().expect("sqlite mutex poisoned");
        let row: Option<(String, String, String, Option<String>, String)> = db
            .query_row(
                "SELECT p.id, p.nick, t.token_hash, p.team, COALESCE(p.role, 'member')
                 FROM principal_tokens t
                 JOIN principals p ON p.id = t.principal_id
                 WHERE t.token_id = ?1 AND t.revoked_at IS NULL",
                params![token_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                },
            )
            .optional()
            .context("principal_tokens lookup")?;

        let Some((id, nick, token_hash, team, role)) = row else {
            return Ok(None);
        };

        if tokens::verify(secret, &token_hash)? {
            Ok(Some(Principal {
                id,
                nick,
                team,
                role,
            }))
        } else {
            Ok(None)
        }
    }

    fn authenticate_legacy(&self, token: &str) -> Result<Principal> {
        if token.starts_with("__hashed__:") {
            return Err(anyhow!("invalid token"));
        }

        let db = self.db.lock().expect("sqlite mutex poisoned");
        let mut statement = db
            .prepare(
                "SELECT id, nick, team, COALESCE(role, 'member')
                 FROM principals
                 WHERE token = ?1 AND token NOT LIKE '__hashed__:%'",
            )
            .context("prepare principal lookup")?;

        let principal = statement
            .query_row(params![token], |row| {
                Ok(Principal {
                    id: row.get(0)?,
                    nick: row.get(1)?,
                    team: row.get(2)?,
                    role: row.get(3)?,
                })
            })
            .map_err(|_| anyhow!("invalid token"))?;

        Ok(principal)
    }

    pub fn register_principal(
        &self,
        raw_nick: &str,
        raw_role: Option<&str>,
        raw_team: Option<&str>,
    ) -> Result<(Principal, String)> {
        let nick = validate_nick(raw_nick)?;
        let id = nick.to_ascii_lowercase();
        let role = normalize_role(raw_role.unwrap_or("member"))?;
        let team = raw_team
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned);

        let (full_token, token_id_hex, secret) = tokens::generate();
        let token_hash = tokens::hash(&secret).context("hash registration token")?;
        let sentinel = format!("__hashed__:{token_id_hex}");

        let mut db = self.db.lock().expect("sqlite mutex poisoned");
        let existing: Option<String> = db
            .query_row(
                "SELECT nick FROM principals WHERE LOWER(id) = LOWER(?1) OR LOWER(nick) = LOWER(?2)",
                params![id, nick],
                |row| row.get(0),
            )
            .optional()
            .context("check existing principal")?;

        if let Some(conflict_nick) = existing {
            return Err(anyhow::Error::new(DuplicatePrincipalError(format!(
                "Agent nick '{conflict_nick}' is already registered; authenticate with its token or choose a new nick"
            ))));
        }

        let tx = db.transaction().context("start registration transaction")?;
        tx.execute(
            "INSERT INTO principals (id, nick, token, team, role, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, unixepoch())",
            params![id, nick, sentinel, team, role],
        )
        .context("insert principal")?;
        tx.execute(
            "INSERT INTO principal_tokens (principal_id, token_id, token_hash, label, created_at)
             VALUES (?1, ?2, ?3, 'register', unixepoch())",
            params![id, token_id_hex, token_hash],
        )
        .context("insert registration token")?;
        tx.commit().context("commit registration transaction")?;

        Ok((
            Principal {
                id,
                nick,
                team,
                role,
            },
            full_token,
        ))
    }

    pub fn set_role(&self, target_name: &str, raw_role: &str) -> Result<Principal> {
        let role = normalize_role(raw_role)?;
        let clean_target = target_name.trim().trim_start_matches('@');
        if clean_target.is_empty() {
            return Err(anyhow!("Target agent nick is required"));
        }

        let db = self.db.lock().expect("sqlite mutex poisoned");
        let principal_id: Option<String> = db
            .query_row(
                "SELECT id FROM principals WHERE LOWER(id) = LOWER(?1) OR LOWER(nick) = LOWER(?1)",
                params![clean_target],
                |row| row.get(0),
            )
            .optional()
            .context("lookup target principal")?;

        let Some(id) = principal_id else {
            return Err(anyhow!("Unknown agent '@{clean_target}'"));
        };

        db.execute(
            "UPDATE principals SET role = ?2 WHERE id = ?1",
            params![id, role],
        )
        .context("update principal role")?;

        Self::get_by_id_locked(&db, &id)?.ok_or_else(|| anyhow!("failed to reload principal"))
    }

    pub fn issue_token(
        &self,
        principal_id: &str,
        label: Option<&str>,
        revoke_existing: bool,
    ) -> Result<String> {
        let (full_token, token_id_hex, secret) = tokens::generate();
        let token_hash = tokens::hash(&secret).context("hash new token")?;
        let clean_label = label
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("cli");

        let mut db = self.db.lock().expect("sqlite mutex poisoned");
        let tx = db.transaction().context("start token issue transaction")?;
        if revoke_existing {
            tx.execute(
                "UPDATE principal_tokens SET revoked_at = unixepoch()
                 WHERE principal_id = ?1 AND revoked_at IS NULL",
                params![principal_id],
            )
            .context("revoke old tokens")?;
        }
        tx.execute(
            "INSERT INTO principal_tokens (principal_id, token_id, token_hash, label, created_at)
             VALUES (?1, ?2, ?3, ?4, unixepoch())",
            params![principal_id, token_id_hex, token_hash, clean_label],
        )
        .context("insert new token")?;
        tx.commit().context("commit token issue")?;
        Ok(full_token)
    }

    /// Owner reset: revoke every credential a principal holds (hashed tokens
    /// and any legacy plaintext token) and issue one fresh token.
    pub fn reissue_all(&self, principal_id: &str) -> Result<String> {
        {
            let db = self.db.lock().expect("sqlite mutex poisoned");
            db.execute(
                "UPDATE principals
                 SET token = '__hashed__:reissued-' || lower(hex(randomblob(16)))
                 WHERE id = ?1 AND token NOT LIKE '__hashed__:%'",
                params![principal_id],
            )
            .context("neutralise legacy token")?;
        }
        // (principal_id, label) is unique, so every reissue needs its own label.
        let unique = uuid::Uuid::new_v4().simple().to_string();
        let label = format!("owner-reissue-{}", &unique[..12]);
        self.issue_token(principal_id, Some(&label), true)
    }

    pub fn resolve_by_name(&self, name: &str) -> Result<Option<Principal>> {
        let clean = name.trim().trim_start_matches('@');
        if clean.is_empty() {
            return Ok(None);
        }
        let db = self.db.lock().expect("sqlite mutex poisoned");
        db.query_row(
            "SELECT id, nick, team, COALESCE(role, 'member')
             FROM principals
             WHERE LOWER(id) = LOWER(?1) OR LOWER(nick) = LOWER(?1)",
            params![clean],
            |row| {
                Ok(Principal {
                    id: row.get(0)?,
                    nick: row.get(1)?,
                    team: row.get(2)?,
                    role: row.get(3)?,
                })
            },
        )
        .optional()
        .context("resolve principal by name")
    }

    pub fn list_principals(&self) -> Result<Vec<Principal>> {
        let db = self.db.lock().expect("sqlite mutex poisoned");
        let mut stmt = db
            .prepare(
                "SELECT id, nick, team, COALESCE(role, 'member')
                 FROM principals
                 ORDER BY LOWER(nick) ASC",
            )
            .context("prepare principals list")?;
        let rows = stmt
            .query_map([], |row| {
                Ok(Principal {
                    id: row.get(0)?,
                    nick: row.get(1)?,
                    team: row.get(2)?,
                    role: row.get(3)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn get_by_id_locked(db: &Connection, id: &str) -> Result<Option<Principal>> {
        db.query_row(
            "SELECT id, nick, team, COALESCE(role, 'member') FROM principals WHERE id = ?1",
            params![id],
            |row| {
                Ok(Principal {
                    id: row.get(0)?,
                    nick: row.get(1)?,
                    team: row.get(2)?,
                    role: row.get(3)?,
                })
            },
        )
        .optional()
        .context("get principal by id")
    }
}

pub fn validate_nick(raw: &str) -> Result<String> {
    let trimmed = raw.trim().trim_start_matches('@');
    if trimmed.is_empty() || trimmed.len() > 32 {
        return Err(anyhow!("Agent nick must be 1-32 characters"));
    }
    if trimmed.starts_with('#') || trimmed.starts_with('&') || trimmed.starts_with(':') {
        return Err(anyhow!("Agent nick cannot start with #, &, or :"));
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        return Err(anyhow!(
            "Agent nick may only contain letters, digits, '_', '-', and '.'"
        ));
    }
    Ok(trimmed.to_string())
}

pub fn normalize_role(raw: &str) -> Result<String> {
    let clean = raw.trim().to_ascii_lowercase();
    if clean.is_empty() || clean.len() > 32 {
        return Err(anyhow!("Role must be 1-32 characters"));
    }
    if !clean
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
    {
        return Err(anyhow!(
            "Role may only contain lowercase letters, digits, '_', and '-'"
        ));
    }
    Ok(clean)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store;
    use rusqlite::params;

    #[test]
    fn legacy_plaintext_token_authenticates() -> Result<()> {
        let db = store::open(":memory:")?;
        let auth = AuthStore::new(db);

        let principal = auth.authenticate("human-token")?;
        assert_eq!(principal.id, "human");
        assert_eq!(principal.nick, "human");
        Ok(())
    }

    #[test]
    fn new_format_token_authenticates() -> Result<()> {
        let db = store::open(":memory:")?;
        let auth = AuthStore::new(db.clone());

        let (full, token_id_hex, secret) = tokens::generate();
        let token_hash = tokens::hash(&secret)?;
        {
            let conn = db.lock().expect("sqlite mutex poisoned");
            conn.execute(
                "INSERT INTO principal_tokens (principal_id, token_id, token_hash, label, created_at)
                 VALUES ('human', ?1, ?2, 'test', unixepoch())",
                params![token_id_hex, token_hash],
            )?;
        }

        let principal = auth.authenticate(&full)?;
        assert_eq!(principal.id, "human");
        assert_eq!(principal.nick, "human");
        Ok(())
    }

    #[test]
    fn revoked_new_token_is_rejected_but_legacy_still_works() -> Result<()> {
        let db = store::open(":memory:")?;
        let auth = AuthStore::new(db.clone());

        let (full, token_id_hex, secret) = tokens::generate();
        let token_hash = tokens::hash(&secret)?;
        {
            let conn = db.lock().expect("sqlite mutex poisoned");
            conn.execute(
                "INSERT INTO principal_tokens (principal_id, token_id, token_hash, label, created_at, revoked_at)
                 VALUES ('human', ?1, ?2, 'test', unixepoch(), unixepoch())",
                params![token_id_hex, token_hash],
            )?;
        }

        assert!(auth.authenticate(&full).is_err());
        // Legacy path still works independently.
        assert!(auth.authenticate("human-token").is_ok());
        Ok(())
    }

    #[test]
    fn invalid_token_errors() {
        let db = store::open(":memory:").expect("open db");
        let auth = AuthStore::new(db);
        assert!(auth.authenticate("does-not-exist").is_err());
        assert!(auth.authenticate("airc_deadbeef_nope").is_err());
    }

    #[test]
    fn legacy_hit_counter_tracks_successful_legacy_auth() -> Result<()> {
        let db = store::open(":memory:")?;
        let auth = AuthStore::new(db);
        assert_eq!(auth.legacy_hit_count(), 0);

        auth.authenticate("human-token")?;
        assert_eq!(auth.legacy_hit_count(), 1);

        auth.authenticate("agent-a-token")?;
        assert_eq!(auth.legacy_hit_count(), 2);

        // Invalid tokens must not bump the counter.
        let _ = auth.authenticate("does-not-exist");
        assert_eq!(auth.legacy_hit_count(), 2);
        Ok(())
    }

    #[test]
    fn register_principal_issues_token_prevents_duplicates_and_updates_roles() -> Result<()> {
        let db = store::open(":memory:")?;
        let auth = AuthStore::new(db);

        let (principal, token) =
            auth.register_principal("Codex-Lead", Some("leader"), Some("core"))?;
        assert_eq!(principal.id, "codex-lead");
        assert_eq!(principal.nick, "Codex-Lead");
        assert_eq!(principal.role, "leader");
        assert!(token.starts_with("airc_"));

        let authed = auth.authenticate(&token)?;
        assert_eq!(authed.id, "codex-lead");
        assert_eq!(authed.role, "leader");

        // Legacy sentinel cannot be used to authenticate directly.
        assert!(auth.authenticate("__hashed__:anything").is_err());

        // Case-insensitive duplicate returns DuplicatePrincipalError.
        let dup_err = auth
            .register_principal("codex-lead", None, None)
            .expect_err("duplicate nick must fail");
        assert!(dup_err.downcast_ref::<DuplicatePrincipalError>().is_some());

        // Role assignment updates and resolves cleanly.
        let updated = auth.set_role("@Codex-Lead", "reviewer")?;
        assert_eq!(updated.role, "reviewer");
        let resolved = auth
            .resolve_by_name("codex-lead")?
            .expect("resolved registered principal");
        assert_eq!(resolved.role, "reviewer");
        Ok(())
    }
}
