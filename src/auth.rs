use anyhow::{anyhow, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::history::unix_timestamp;
use crate::logging::{EventForwarder, Severity};
use crate::store::SharedConnection;
use crate::tokens;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Read = 0,
    Write = 1,
    Pm = 2,
    Admin = 3,
}

impl Scope {
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "read" | "read-only" => Ok(Self::Read),
            "write" | "builder" => Ok(Self::Write),
            "pm" => Ok(Self::Pm),
            "admin" => Ok(Self::Admin),
            other => Err(anyhow!(
                "unknown token scope '{other}': expected read, write, pm, or admin (aliases: read-only, builder)"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Pm => "pm",
            Self::Admin => "admin",
        }
    }

    pub fn implies(self, required: Self) -> bool {
        self >= required
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

pub fn normalize_scopes(raw: Option<&[String]>) -> Result<Vec<Scope>> {
    let Some(list) = raw else {
        return Ok(vec![Scope::Admin]);
    };
    if list.is_empty() {
        return Err(anyhow!("at least one token scope is required"));
    }
    let mut set = BTreeSet::new();
    for item in list {
        set.insert(Scope::parse(item)?);
    }
    Ok(set.into_iter().collect())
}

fn encode_scopes_column(scopes: &[Scope]) -> String {
    serde_json::to_string(scopes).unwrap_or_else(|_| "[\"admin\"]".to_string())
}

fn parse_scopes_column(raw: &str) -> Result<Vec<Scope>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(vec![Scope::Admin]);
    }
    if let Ok(items) = serde_json::from_str::<Vec<String>>(trimmed) {
        return normalize_scopes(Some(&items));
    }
    let items: Vec<String> = trimmed
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    normalize_scopes(Some(&items))
}

fn compute_expiry(
    now: i64,
    explicit_ttl: Option<u64>,
    default_ttl: Option<u64>,
) -> Result<(Option<u64>, Option<i64>)> {
    if explicit_ttl == Some(0) {
        return Err(anyhow!("ttl_seconds must be at least 1"));
    }
    let effective = explicit_ttl.or(default_ttl);
    if effective == Some(0) {
        return Err(anyhow!("ttl_seconds must be at least 1"));
    }
    let expires_at = effective
        .map(|ttl| {
            i64::try_from(ttl)
                .ok()
                .and_then(|secs| now.checked_add(secs))
                .ok_or_else(|| anyhow!("ttl_seconds overflows unix timestamp"))
        })
        .transpose()?;
    Ok((effective, expires_at))
}

#[derive(Debug)]
pub struct DuplicatePrincipalError(pub String);

impl fmt::Display for DuplicatePrincipalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DuplicatePrincipalError {}

#[derive(Debug)]
pub struct TokenExpiredError;

impl fmt::Display for TokenExpiredError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("bearer token has expired")
    }
}

impl std::error::Error for TokenExpiredError {}

#[derive(Debug)]
pub struct DuplicateTokenLabelError(pub String);

impl fmt::Display for DuplicateTokenLabelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DuplicateTokenLabelError {}

#[derive(Clone, Debug, Serialize)]
pub struct Principal {
    pub id: String,
    pub nick: String,
    pub team: Option<String>,
    pub role: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct AuthTokenInfo {
    pub token_id: Option<String>,
    pub label: Option<String>,
    pub scopes: Vec<Scope>,
    pub ttl_seconds: Option<u64>,
    pub expires_at: Option<i64>,
}

impl AuthTokenInfo {
    pub fn has_scope(&self, required: Scope) -> bool {
        self.scopes.iter().any(|s| s.implies(required))
    }

    pub fn is_expired_at(&self, now: i64) -> bool {
        self.expires_at.is_some_and(|exp| exp <= now)
    }
}

#[derive(Clone, Debug)]
pub struct AuthContext {
    pub principal: Principal,
    pub token: AuthTokenInfo,
}

#[derive(Clone, Debug, Serialize)]
pub struct IssuedToken {
    pub token: String,
    pub token_id: String,
    pub label: String,
    pub scopes: Vec<Scope>,
    pub ttl_seconds: Option<u64>,
    pub expires_at: Option<i64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct TokenRecord {
    pub token_id: String,
    pub principal_id: String,
    pub label: Option<String>,
    pub scopes: Vec<Scope>,
    pub ttl_seconds: Option<u64>,
    pub created_at: i64,
    pub expires_at: Option<i64>,
    pub revoked_at: Option<i64>,
    pub active: bool,
}

#[derive(Clone)]
pub struct AuthStore {
    db: SharedConnection,
    legacy_hits: Arc<AtomicU64>,
    logger: EventForwarder,
    default_ttl_seconds: Option<u64>,
}

impl AuthStore {
    #[allow(dead_code)]
    pub fn new(db: SharedConnection) -> Self {
        Self::with_logger(db, EventForwarder::default())
    }

    pub fn with_logger(db: SharedConnection, logger: EventForwarder) -> Self {
        Self::with_config(db, logger, None)
    }

    pub fn with_config(
        db: SharedConnection,
        logger: EventForwarder,
        default_ttl_seconds: Option<u64>,
    ) -> Self {
        Self {
            db,
            legacy_hits: Arc::new(AtomicU64::new(0)),
            logger,
            default_ttl_seconds: default_ttl_seconds.filter(|ttl| *ttl > 0),
        }
    }

    pub fn default_ttl_seconds(&self) -> Option<u64> {
        self.default_ttl_seconds
    }

    #[allow(dead_code)]
    pub fn with_default_ttl(mut self, ttl_seconds: Option<u64>) -> Self {
        self.default_ttl_seconds = ttl_seconds.filter(|ttl| *ttl > 0);
        self
    }

    /// Authenticate an incoming token and return the principal.
    #[cfg(test)]
    pub fn authenticate(&self, token: &str) -> Result<Principal> {
        Ok(self.authenticate_context(token)?.principal)
    }

    /// Authenticate an incoming token and return both the principal and the
    /// token's scopes / expiry metadata. Tries the new `principal_tokens` path
    /// first when the token has the `airc_` prefix, falls back to the legacy
    /// `principals.token` column so existing deployments keep working during
    /// the migration window.
    pub fn authenticate_context(&self, token: &str) -> Result<AuthContext> {
        if let Some((token_id, secret)) = tokens::parse(token) {
            if let Some(ctx) = self.authenticate_new(&token_id, &secret)? {
                return Ok(ctx);
            }
        }

        let principal = self.authenticate_legacy(token)?;
        let total = self.legacy_hits.fetch_add(1, Ordering::Relaxed) + 1;
        let msg = format!(
            "auth: legacy plaintext token accepted for principal {} (legacy_hits={})",
            principal.nick, total,
        );
        eprintln!("{msg}");
        self.logger.log_security(
            Severity::Warning,
            "auth.legacy_token",
            "success",
            Some(&principal.id),
            Some(&principal.id),
            None,
            msg,
        );
        Ok(AuthContext {
            principal,
            token: AuthTokenInfo {
                token_id: None,
                label: None,
                scopes: vec![Scope::Admin],
                ttl_seconds: None,
                expires_at: None,
            },
        })
    }

    /// Total number of successful legacy-path authentications since startup.
    /// Used to gauge when the legacy column in `principals` can be dropped
    /// (PR C). Exposed for future CLI/status endpoints.
    #[allow(dead_code)]
    pub fn legacy_hit_count(&self) -> u64 {
        self.legacy_hits.load(Ordering::Relaxed)
    }

    fn authenticate_new(&self, token_id: &str, secret: &str) -> Result<Option<AuthContext>> {
        struct TokenAuthRow {
            id: String,
            nick: String,
            token_hash: String,
            team: Option<String>,
            role: String,
            scopes_raw: String,
            ttl_raw: Option<i64>,
            expires_at: Option<i64>,
            label: Option<String>,
        }

        let now = unix_timestamp();
        let row: Option<TokenAuthRow> = {
            let db = self.db.read()?;
            db.query_row(
                "SELECT p.id, p.nick, t.token_hash, p.team, COALESCE(p.role, 'member'),
                        t.scopes, t.ttl_seconds, t.expires_at, t.label
                 FROM principal_tokens t
                 JOIN principals p ON p.id = t.principal_id
                 WHERE t.token_id = ?1
                   AND t.revoked_at IS NULL
                   AND (t.expires_at IS NULL OR t.expires_at > ?2)",
                params![token_id, now],
                |row| {
                    Ok(TokenAuthRow {
                        id: row.get(0)?,
                        nick: row.get(1)?,
                        token_hash: row.get(2)?,
                        team: row.get(3)?,
                        role: row.get(4)?,
                        scopes_raw: row.get(5)?,
                        ttl_raw: row.get(6)?,
                        expires_at: row.get(7)?,
                        label: row.get(8)?,
                    })
                },
            )
            .optional()
            .context("principal_tokens lookup")?
        };

        if let Some(row) = row {
            if tokens::verify(secret, &row.token_hash)? {
                let scopes = parse_scopes_column(&row.scopes_raw)?;
                let ttl_seconds = row
                    .ttl_raw
                    .and_then(|v| u64::try_from(v).ok())
                    .filter(|v| *v > 0);
                return Ok(Some(AuthContext {
                    principal: Principal {
                        id: row.id,
                        nick: row.nick,
                        team: row.team,
                        role: row.role,
                    },
                    token: AuthTokenInfo {
                        token_id: Some(token_id.to_string()),
                        label: row.label,
                        scopes,
                        ttl_seconds,
                        expires_at: row.expires_at,
                    },
                }));
            }
            return Ok(None);
        }

        // Distinguish expired token (valid secret, past expires_at) from unknown/revoked.
        let expired_row: Option<(String, String, String)> = {
            let db = self.db.read()?;
            db.query_row(
                "SELECT p.id, p.nick, t.token_hash
                 FROM principal_tokens t
                 JOIN principals p ON p.id = t.principal_id
                 WHERE t.token_id = ?1
                   AND t.revoked_at IS NULL
                   AND t.expires_at IS NOT NULL
                   AND t.expires_at <= ?2",
                params![token_id, now],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .context("principal_tokens expired lookup")?
        };
        if let Some((id, nick, token_hash)) = expired_row {
            if tokens::verify(secret, &token_hash)? {
                self.logger.log_security(
                    Severity::Warning,
                    "auth.token_expired",
                    "failure",
                    Some(&id),
                    Some(&id),
                    None,
                    format!("Expired bearer token rejected for principal @{nick} (token_id={token_id})"),
                );
                return Err(anyhow::Error::new(TokenExpiredError));
            }
        }

        Ok(None)
    }

    /// Re-check whether an active `token_id` is still unrevoked and unexpired.
    pub fn is_token_active(&self, token_id: &str) -> Result<Option<Option<i64>>> {
        let now = unix_timestamp();
        let db = self.db.read()?;
        db.query_row(
            "SELECT expires_at
             FROM principal_tokens
             WHERE token_id = ?1
               AND revoked_at IS NULL
               AND (expires_at IS NULL OR expires_at > ?2)",
            params![token_id, now],
            |row| row.get::<_, Option<i64>>(0),
        )
        .optional()
        .context("check token active status")
    }

    fn authenticate_legacy(&self, token: &str) -> Result<Principal> {
        if token.starts_with("__hashed__:") {
            return Err(anyhow!("invalid token"));
        }

        let db = self.db.read()?;
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

    #[cfg(test)]
    pub fn register_principal(
        &self,
        raw_nick: &str,
        raw_role: Option<&str>,
        raw_team: Option<&str>,
    ) -> Result<(Principal, String)> {
        let (principal, issued) =
            self.register_principal_with_options(raw_nick, raw_role, raw_team, None, None)?;
        Ok((principal, issued.token))
    }

    pub fn register_principal_with_options(
        &self,
        raw_nick: &str,
        raw_role: Option<&str>,
        raw_team: Option<&str>,
        raw_scopes: Option<&[String]>,
        ttl_seconds: Option<u64>,
    ) -> Result<(Principal, IssuedToken)> {
        let effective_ttl = match ttl_seconds {
            Some(0) => return Err(anyhow!("ttl_seconds must be at least 1")),
            Some(ttl) => Some(ttl),
            None => self.default_ttl_seconds,
        };
        let mut db = self.db.write()?;
        let tx = db.transaction().context("start registration transaction")?;
        let created = Self::register_principal_on_with_options(
            &tx,
            raw_nick,
            raw_role,
            raw_team,
            raw_scopes,
            effective_ttl,
        )?;
        tx.commit().context("commit registration transaction")?;
        Ok(created)
    }

    /// Register inside the caller's transaction with explicit token scopes and TTL.
    pub(crate) fn register_principal_on_with_options(
        db: &Connection,
        raw_nick: &str,
        raw_role: Option<&str>,
        raw_team: Option<&str>,
        raw_scopes: Option<&[String]>,
        ttl_seconds: Option<u64>,
    ) -> Result<(Principal, IssuedToken)> {
        let nick = validate_nick(raw_nick)?;
        let id = nick.to_ascii_lowercase();
        let role = normalize_role(raw_role.unwrap_or("member"))?;
        let team = raw_team
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned);
        let scopes = normalize_scopes(raw_scopes)?;
        let scopes_json = encode_scopes_column(&scopes);
        let now = unix_timestamp();
        let (effective_ttl, expires_at) = compute_expiry(now, ttl_seconds, None)?;

        let (full_token, token_id_hex, secret) = tokens::generate();
        let token_hash = tokens::hash(&secret).context("hash registration token")?;
        let sentinel = format!("__hashed__:{token_id_hex}");

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

        db.execute(
            "INSERT INTO principals (id, nick, token, team, role, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, nick, sentinel, team, role, now],
        )
        .context("insert principal")?;
        let label = "register".to_string();
        let ttl_i64 = effective_ttl.map(i64::try_from).transpose()?;
        db.execute(
            "INSERT INTO principal_tokens (principal_id, token_id, token_hash, label, scopes, ttl_seconds, expires_at, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                id,
                token_id_hex,
                token_hash,
                label,
                scopes_json,
                ttl_i64,
                expires_at,
                now
            ],
        )
        .context("insert registration token")?;

        Ok((
            Principal {
                id,
                nick,
                team,
                role,
            },
            IssuedToken {
                token: full_token,
                token_id: token_id_hex,
                label,
                scopes,
                ttl_seconds: effective_ttl,
                expires_at,
            },
        ))
    }

    pub fn set_role(&self, target_name: &str, raw_role: &str) -> Result<Principal> {
        let role = normalize_role(raw_role)?;
        let clean_target = target_name.trim().trim_start_matches('@');
        if clean_target.is_empty() {
            return Err(anyhow!("Target agent nick is required"));
        }

        let db = self.db.write()?;
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

    #[cfg(test)]
    pub fn issue_token(
        &self,
        principal_id: &str,
        label: Option<&str>,
        revoke_existing: bool,
    ) -> Result<String> {
        let issued =
            self.issue_token_with_options(principal_id, label, revoke_existing, None, None)?;
        Ok(issued.token)
    }

    pub fn issue_token_with_options(
        &self,
        principal_id: &str,
        label: Option<&str>,
        revoke_existing: bool,
        raw_scopes: Option<&[String]>,
        ttl_seconds: Option<u64>,
    ) -> Result<IssuedToken> {
        let scopes = normalize_scopes(raw_scopes)?;
        Self::issue_token_with_scopes(
            self,
            principal_id,
            label,
            revoke_existing,
            &scopes,
            ttl_seconds,
        )
    }

    pub fn issue_token_with_scopes(
        &self,
        principal_id: &str,
        label: Option<&str>,
        revoke_existing: bool,
        scopes: &[Scope],
        ttl_seconds: Option<u64>,
    ) -> Result<IssuedToken> {
        if scopes.is_empty() {
            return Err(anyhow!("at least one token scope is required"));
        }
        let now = unix_timestamp();
        let (effective_ttl, expires_at) =
            compute_expiry(now, ttl_seconds, self.default_ttl_seconds)?;
        let ttl_i64 = effective_ttl.map(i64::try_from).transpose()?;
        let scopes_json = encode_scopes_column(scopes);

        let (full_token, token_id_hex, secret) = tokens::generate();
        let token_hash = tokens::hash(&secret).context("hash new token")?;
        let clean_label = label
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| format!("token-{token_id_hex}"));

        let mut db = self.db.write()?;
        let tx = db.transaction().context("start token issue transaction")?;
        let exists: Option<String> = tx
            .query_row(
                "SELECT id FROM principals WHERE id = ?1",
                params![principal_id],
                |row| row.get(0),
            )
            .optional()
            .context("verify principal for token issuance")?;
        if exists.is_none() {
            return Err(anyhow!("unknown principal '{principal_id}'"));
        }
        if revoke_existing {
            tx.execute(
                "UPDATE principal_tokens SET revoked_at = ?2
                 WHERE principal_id = ?1 AND revoked_at IS NULL",
                params![principal_id, now],
            )
            .context("revoke old tokens")?;
        }
        tx.execute(
            "INSERT INTO principal_tokens (principal_id, token_id, token_hash, label, scopes, ttl_seconds, expires_at, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                principal_id,
                token_id_hex,
                token_hash,
                clean_label,
                scopes_json,
                ttl_i64,
                expires_at,
                now
            ],
        )
        .map_err(|err| {
            if let rusqlite::Error::SqliteFailure(sqlite_err, _) = &err {
                if sqlite_err.code == rusqlite::ErrorCode::ConstraintViolation {
                    return anyhow::Error::new(DuplicateTokenLabelError(format!(
                        "token label '{clean_label}' already exists for principal '{principal_id}'"
                    )));
                }
            }
            anyhow::Error::new(err).context("insert new token")
        })?;
        tx.commit().context("commit token issue")?;
        Ok(IssuedToken {
            token: full_token,
            token_id: token_id_hex,
            label: clean_label,
            scopes: scopes.to_vec(),
            ttl_seconds: effective_ttl,
            expires_at,
        })
    }

    /// Owner reset: revoke every credential a principal holds (hashed tokens
    /// and any legacy plaintext token) and issue one fresh token.
    #[cfg(test)]
    pub fn reissue_all(&self, principal_id: &str) -> Result<String> {
        Ok(self.reissue_all_with_options(principal_id, None, None)?.token)
    }

    pub fn reissue_all_with_options(
        &self,
        principal_id: &str,
        raw_scopes: Option<&[String]>,
        ttl_seconds: Option<u64>,
    ) -> Result<IssuedToken> {
        let scopes = normalize_scopes(raw_scopes)?;
        let scopes_json = encode_scopes_column(&scopes);
        let now = unix_timestamp();
        let (effective_ttl, expires_at) =
            compute_expiry(now, ttl_seconds, self.default_ttl_seconds)?;
        let ttl_i64 = effective_ttl.map(i64::try_from).transpose()?;

        let (full_token, token_id, secret) = tokens::generate();
        let token_hash = tokens::hash(&secret).context("hash reissued token")?;
        // (principal_id, label) is unique, so every reissue needs its own label.
        let label = format!("owner-reissue-{token_id}");
        let mut db = self.db.write()?;
        let tx = db
            .transaction()
            .context("start token reissue transaction")?;
        let updated = tx
            .execute(
                "UPDATE principals SET token = ?2 WHERE id = ?1",
                params![principal_id, format!("__hashed__:{token_id}")],
            )
            .context("neutralise legacy token")?;
        if updated != 1 {
            return Err(anyhow!("unknown principal"));
        }
        tx.execute(
            "UPDATE principal_tokens SET revoked_at = ?2
             WHERE principal_id = ?1 AND revoked_at IS NULL",
            params![principal_id, now],
        )
        .context("revoke old tokens")?;
        tx.execute(
            "INSERT INTO principal_tokens (principal_id, token_id, token_hash, label, scopes, ttl_seconds, expires_at, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                principal_id,
                token_id,
                token_hash,
                label,
                scopes_json,
                ttl_i64,
                expires_at,
                now
            ],
        )
        .context("insert reissued token")?;
        tx.commit().context("commit token reissue")?;
        Ok(IssuedToken {
            token: full_token,
            token_id,
            label,
            scopes,
            ttl_seconds: effective_ttl,
            expires_at,
        })
    }

    /// Refresh an active bearer token's expiry (and optionally rotate the secret).
    /// Preserves the token's existing scopes so refresh never escalates privileges.
    pub fn refresh_token(
        &self,
        raw_token: &str,
        ttl_seconds: Option<u64>,
        rotate: bool,
    ) -> Result<(Principal, IssuedToken)> {
        if ttl_seconds == Some(0) {
            return Err(anyhow!("ttl_seconds must be at least 1"));
        }
        let ctx = self.authenticate_context(raw_token)?;
        let now = unix_timestamp();

        let Some(token_id) = ctx.token.token_id.clone() else {
            // Legacy plaintext token: non-expiring unless rotated into a hashed token.
            if rotate {
                let issued = self.issue_token_with_scopes(
                    &ctx.principal.id,
                    None,
                    false,
                    &ctx.token.scopes,
                    ttl_seconds,
                )?;
                return Ok((ctx.principal, issued));
            }
            return Ok((
                ctx.principal,
                IssuedToken {
                    token: raw_token.to_string(),
                    token_id: "legacy".to_string(),
                    label: "legacy".to_string(),
                    scopes: ctx.token.scopes,
                    ttl_seconds: None,
                    expires_at: None,
                },
            ));
        };

        let effective_ttl = ttl_seconds
            .or(ctx.token.ttl_seconds)
            .or(self.default_ttl_seconds);

        if rotate {
            let (full_token, new_token_id, secret) = tokens::generate();
            let token_hash = tokens::hash(&secret).context("hash refreshed token")?;
            let (stored_ttl, new_expires_at) = compute_expiry(now, effective_ttl, None)?;
            let ttl_i64 = stored_ttl.map(i64::try_from).transpose()?;
            let scopes_json = encode_scopes_column(&ctx.token.scopes);
            let label = format!("refresh-{new_token_id}");

            let mut db = self.db.write()?;
            let tx = db
                .transaction()
                .context("start token refresh rotation transaction")?;
            let revoked = tx
                .execute(
                    "UPDATE principal_tokens SET revoked_at = ?2
                     WHERE token_id = ?1 AND revoked_at IS NULL AND (expires_at IS NULL OR expires_at > ?2)",
                    params![token_id, now],
                )
                .context("revoke old token during refresh")?;
            if revoked != 1 {
                return Err(anyhow!("token is no longer active"));
            }
            tx.execute(
                "INSERT INTO principal_tokens (principal_id, token_id, token_hash, label, scopes, ttl_seconds, expires_at, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    ctx.principal.id,
                    new_token_id,
                    token_hash,
                    label,
                    scopes_json,
                    ttl_i64,
                    new_expires_at,
                    now
                ],
            )
            .context("insert rotated refresh token")?;
            tx.commit().context("commit token refresh rotation")?;

            return Ok((
                ctx.principal,
                IssuedToken {
                    token: full_token,
                    token_id: new_token_id,
                    label,
                    scopes: ctx.token.scopes,
                    ttl_seconds: stored_ttl,
                    expires_at: new_expires_at,
                },
            ));
        }

        let (stored_ttl, new_expires_at) = if let Some(ttl) = effective_ttl {
            let (_, exp) = compute_expiry(now, Some(ttl), None)?;
            (Some(ttl), exp)
        } else {
            (ctx.token.ttl_seconds, ctx.token.expires_at)
        };
        let ttl_i64 = stored_ttl.map(i64::try_from).transpose()?;

        let db = self.db.write()?;
        let updated = db
            .execute(
                "UPDATE principal_tokens
                 SET expires_at = ?2, ttl_seconds = ?3
                 WHERE token_id = ?1 AND revoked_at IS NULL AND (expires_at IS NULL OR expires_at > ?4)",
                params![token_id, new_expires_at, ttl_i64, now],
            )
            .context("refresh token expiry")?;
        if updated != 1 {
            return Err(anyhow!("token is no longer active"));
        }

        Ok((
            ctx.principal,
            IssuedToken {
                token: raw_token.to_string(),
                token_id,
                label: ctx.token.label.unwrap_or_default(),
                scopes: ctx.token.scopes,
                ttl_seconds: stored_ttl,
                expires_at: new_expires_at,
            },
        ))
    }

    pub fn list_tokens(&self, principal_id: &str) -> Result<Vec<TokenRecord>> {
        let now = unix_timestamp();
        let db = self.db.read()?;
        let mut stmt = db
            .prepare(
                "SELECT token_id, principal_id, label, scopes, ttl_seconds, created_at, expires_at, revoked_at
                 FROM principal_tokens
                 WHERE principal_id = ?1
                 ORDER BY created_at DESC, id DESC",
            )
            .context("prepare list tokens")?;
        let rows = stmt
            .query_map(params![principal_id], |row| {
                let scopes_raw: String = row.get(3)?;
                let ttl_raw: Option<i64> = row.get(4)?;
                let expires_at: Option<i64> = row.get(6)?;
                let revoked_at: Option<i64> = row.get(7)?;
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    scopes_raw,
                    ttl_raw,
                    row.get::<_, i64>(5)?,
                    expires_at,
                    revoked_at,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let mut records = Vec::with_capacity(rows.len());
        for (
            token_id,
            principal_id,
            label,
            scopes_raw,
            ttl_raw,
            created_at,
            expires_at,
            revoked_at,
        ) in rows
        {
            let scopes = parse_scopes_column(&scopes_raw)?;
            let ttl_seconds = ttl_raw.and_then(|v| u64::try_from(v).ok()).filter(|v| *v > 0);
            let active = revoked_at.is_none() && expires_at.is_none_or(|exp| exp > now);
            records.push(TokenRecord {
                token_id,
                principal_id,
                label,
                scopes,
                ttl_seconds,
                created_at,
                expires_at,
                revoked_at,
                active,
            });
        }
        Ok(records)
    }

    pub fn revoke_token(&self, token_id: &str) -> Result<bool> {
        let now = unix_timestamp();
        let db = self.db.write()?;
        let updated = db
            .execute(
                "UPDATE principal_tokens SET revoked_at = ?2
                 WHERE token_id = ?1 AND revoked_at IS NULL",
                params![token_id.trim(), now],
            )
            .context("revoke token")?;
        Ok(updated > 0)
    }

    pub fn resolve_by_name(&self, name: &str) -> Result<Option<Principal>> {
        let clean = name.trim().trim_start_matches('@');
        if clean.is_empty() {
            return Ok(None);
        }
        let db = self.db.read()?;
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
        let db = self.db.read()?;
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
    fn reissue_retires_every_credential_and_can_be_repeated() -> Result<()> {
        let db = store::open(":memory:")?;
        let auth = AuthStore::new(db.clone());
        let extra = auth.issue_token("agent-a", Some("extra"), false)?;
        let first = auth.reissue_all("agent-a")?;
        assert!(auth.authenticate("agent-a-token").is_err());
        assert!(auth.authenticate(&extra).is_err());
        assert_eq!(auth.authenticate(&first)?.id, "agent-a");
        let second = auth.reissue_all("agent-a")?;
        assert!(auth.authenticate(&first).is_err());
        assert_eq!(auth.authenticate(&second)?.id, "agent-a");
        assert_eq!(auth.authenticate("agent-b-token")?.id, "agent-b");
        let conn = db.lock().unwrap();
        let active: i64 = conn.query_row(
            "SELECT COUNT(*) FROM principal_tokens WHERE principal_id = 'agent-a' AND revoked_at IS NULL",
            [], |row| row.get(0),
        )?;
        assert_eq!(active, 1);
        let sentinel: String = conn.query_row(
            "SELECT token FROM principals WHERE id = 'agent-a'",
            [],
            |row| row.get(0),
        )?;
        drop(conn);
        assert!(sentinel.starts_with("__hashed__:"));
        assert!(auth.authenticate(&sentinel).is_err());
        Ok(())
    }

    #[test]
    fn reissue_rolls_back_every_mutation_when_any_statement_fails() -> Result<()> {
        let db = store::open(":memory:")?;
        let auth = AuthStore::new(db.clone());
        let extra = auth.issue_token("agent-a", Some("extra"), false)?;
        let snapshot = || -> Result<_> {
            let conn = db.lock().unwrap();
            let legacy: String = conn.query_row(
                "SELECT token FROM principals WHERE id = 'agent-a'",
                [],
                |row| row.get(0),
            )?;
            let mut stmt = conn.prepare(
                "SELECT id, token_id, token_hash, label, created_at, revoked_at
                 FROM principal_tokens WHERE principal_id = 'agent-a' ORDER BY id",
            )?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, Option<i64>>(5)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok((legacy, rows))
        };
        let before = snapshot()?;
        for operation in [
            "UPDATE ON principals",
            "UPDATE ON principal_tokens",
            "INSERT ON principal_tokens",
        ] {
            db.lock().unwrap().execute_batch(&format!(
                "CREATE TEMP TRIGGER fail_reissue BEFORE {operation}
                 BEGIN SELECT RAISE(ABORT, 'injected reissue failure'); END;"
            ))?;
            let error = auth.reissue_all("agent-a").expect_err("injected failure");
            assert!(format!("{error:#}").contains("injected reissue failure"));
            assert_eq!(snapshot()?, before, "failed at {operation}");
            assert_eq!(auth.authenticate("agent-a-token")?.id, "agent-a");
            assert_eq!(auth.authenticate(&extra)?.id, "agent-a");
            db.lock()
                .unwrap()
                .execute_batch("DROP TRIGGER fail_reissue")?;
        }
        Ok(())
    }

    #[test]
    fn reissue_of_unknown_principal_does_not_insert_a_token() -> Result<()> {
        let db = store::open(":memory:")?;
        let auth = AuthStore::new(db.clone());
        assert!(auth.reissue_all("missing").is_err());
        let count: i64 = db.lock().unwrap().query_row(
            "SELECT COUNT(*) FROM principal_tokens WHERE principal_id = 'missing'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(count, 0);
        Ok(())
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

    #[test]
    fn scope_parsing_aliases_and_hierarchy_work() -> Result<()> {
        assert_eq!(Scope::parse("read")?, Scope::Read);
        assert_eq!(Scope::parse("READ-ONLY")?, Scope::Read);
        assert_eq!(Scope::parse(" write ")?, Scope::Write);
        assert_eq!(Scope::parse("builder")?, Scope::Write);
        assert_eq!(Scope::parse("pm")?, Scope::Pm);
        assert_eq!(Scope::parse("admin")?, Scope::Admin);
        assert!(Scope::parse("superuser").is_err());

        assert!(Scope::Admin.implies(Scope::Pm));
        assert!(Scope::Admin.implies(Scope::Read));
        assert!(Scope::Pm.implies(Scope::Write));
        assert!(Scope::Write.implies(Scope::Read));
        assert!(!Scope::Write.implies(Scope::Pm));
        assert!(!Scope::Read.implies(Scope::Write));

        assert_eq!(normalize_scopes(None)?, vec![Scope::Admin]);
        assert!(normalize_scopes(Some(&[])).is_err());
        let parsed = normalize_scopes(Some(&[
            "read-only".to_string(),
            "builder".to_string(),
            "read".to_string(),
        ]))?;
        assert_eq!(parsed, vec![Scope::Read, Scope::Write]);
        Ok(())
    }

    #[test]
    fn token_ttl_expiry_and_default_ttl_are_enforced() -> Result<()> {
        let db = store::open(":memory:")?;
        let auth = AuthStore::new(db.clone()).with_default_ttl(Some(3600));

        // Legacy/seed tokens remain non-expiring and carry admin scope.
        let legacy_ctx = auth.authenticate_context("agent-a-token")?;
        assert_eq!(legacy_ctx.principal.id, "agent-a");
        assert_eq!(legacy_ctx.token.scopes, vec![Scope::Admin]);
        assert_eq!(legacy_ctx.token.expires_at, None);

        // Zero TTL is rejected.
        let zero_err = auth
            .issue_token_with_options("agent-a", Some("zero"), false, None, Some(0))
            .expect_err("zero TTL must be rejected");
        assert!(zero_err.to_string().contains("ttl_seconds"));

        // Newly issued token inherits server default TTL when unspecified.
        let (_, issued) = auth.register_principal_with_options(
            "Scoped-Builder",
            Some("builder"),
            None,
            Some(&["builder".to_string()]),
            None,
        )?;
        assert_eq!(issued.scopes, vec![Scope::Write]);
        assert_eq!(issued.ttl_seconds, Some(3600));
        assert!(issued.expires_at.is_some());

        let ctx = auth.authenticate_context(&issued.token)?;
        assert_eq!(ctx.token.scopes, vec![Scope::Write]);
        assert!(ctx.token.has_scope(Scope::Read));
        assert!(ctx.token.has_scope(Scope::Write));
        assert!(!ctx.token.has_scope(Scope::Pm));

        // Force expiration in DB and verify authenticate / authenticate_context reject with TokenExpiredError.
        db.write()?.execute(
            "UPDATE principal_tokens SET expires_at = 1 WHERE token_id = ?1",
            params![issued.token_id],
        )?;
        let exp_err = auth
            .authenticate_context(&issued.token)
            .expect_err("expired token must be rejected");
        assert!(exp_err.downcast_ref::<TokenExpiredError>().is_some());
        assert!(auth
            .authenticate(&issued.token)
            .expect_err("authenticate rejects expired token")
            .downcast_ref::<TokenExpiredError>()
            .is_some());
        Ok(())
    }

    #[test]
    fn token_refresh_list_and_revoke_preserve_scopes_and_reject_expired() -> Result<()> {
        let db = store::open(":memory:")?;
        let auth = AuthStore::new(db.clone());
        let (principal, _) = auth.register_principal("Refreshable", Some("builder"), None)?;

        let issued = auth.issue_token_with_options(
            &principal.id,
            Some("ci-worker"),
            false,
            Some(&["read-only".to_string()]),
            Some(120),
        )?;
        assert_eq!(issued.scopes, vec![Scope::Read]);
        assert_eq!(issued.ttl_seconds, Some(120));

        let listed = auth.list_tokens(&principal.id)?;
        assert!(listed
            .iter()
            .any(|t| t.token_id == issued.token_id && t.scopes == vec![Scope::Read]));

        // Non-rotating refresh extends expiry and preserves scopes.
        let (ref_principal, refreshed) = auth.refresh_token(&issued.token, Some(600), false)?;
        assert_eq!(ref_principal.id, principal.id);
        assert_eq!(refreshed.token_id, issued.token_id);
        assert_eq!(refreshed.token, issued.token);
        assert_eq!(refreshed.scopes, vec![Scope::Read]);
        assert_eq!(refreshed.ttl_seconds, Some(600));
        assert!(refreshed.expires_at >= issued.expires_at);

        // Rotating refresh without explicit ttl preserves original ttl_seconds and scopes,
        // while retiring the previous token secret.
        let (_, rotated) = auth.refresh_token(&issued.token, None, true)?;
        assert_ne!(rotated.token_id, issued.token_id);
        assert_ne!(rotated.token, issued.token);
        assert_eq!(rotated.scopes, vec![Scope::Read]);
        assert_eq!(rotated.ttl_seconds, Some(600));
        assert!(auth.authenticate_context(&issued.token).is_err());
        assert_eq!(
            auth.authenticate_context(&rotated.token)?.token.scopes,
            vec![Scope::Read]
        );

        // Revoking by token_id prevents further use and refresh.
        assert!(auth.revoke_token(&rotated.token_id)?);
        assert!(!auth.revoke_token(&rotated.token_id)?);
        assert!(auth.authenticate_context(&rotated.token).is_err());

        // Expired token cannot be refreshed.
        let expiring = auth.issue_token_with_options(
            &principal.id,
            Some("soon-expired"),
            false,
            Some(&["pm".to_string()]),
            Some(60),
        )?;
        db.write()?.execute(
            "UPDATE principal_tokens SET expires_at = 1 WHERE token_id = ?1",
            params![expiring.token_id],
        )?;
        let refresh_err = auth
            .refresh_token(&expiring.token, Some(600), false)
            .expect_err("expired token cannot be refreshed");
        assert!(refresh_err.downcast_ref::<TokenExpiredError>().is_some());
        Ok(())
    }
}
