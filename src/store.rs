use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::tokens;

pub type SharedConnection = Arc<Mutex<Connection>>;

/// Label used to mark `principal_tokens` rows that were backfilled from the
/// legacy `principals.token` column. The uniqueness constraint below prevents
/// double backfills even if `init` runs repeatedly.
const MIGRATED_LEGACY_LABEL: &str = "migrated-legacy";

/// Label used for rows created alongside seeded demo principals.
const SEED_LABEL: &str = "seed";

/// Demo/test principals whose tokens are published in the README. They are
/// only usable on in-memory databases (tests) or when `AIRCD_SEED_DEMO=1`.
const DEMO_PRINCIPALS: &[(&str, &str, &str)] = &[
    ("human", "human-token", "demo"),
    ("agent-a", "agent-a-token", "demo"),
    ("agent-b", "agent-b-token", "demo"),
    ("agent-c", "agent-c-token", "demo"),
    ("agent-1", "test-token-1", "test"),
    ("agent-2", "test-token-2", "test"),
    ("agent-3", "test-token-3", "test"),
];

pub fn open(path: impl AsRef<Path>) -> Result<SharedConnection> {
    let seed_demo = path.as_ref() == Path::new(":memory:") || demo_seed_requested();
    let connection = Connection::open(path).context("open sqlite database")?;
    init_with(&connection, seed_demo)?;
    Ok(Arc::new(Mutex::new(connection)))
}

fn demo_seed_requested() -> bool {
    matches!(
        std::env::var("AIRCD_SEED_DEMO").ok().as_deref(),
        Some("1") | Some("true") | Some("TRUE") | Some("yes")
    )
}

#[cfg(test)]
fn init(connection: &Connection) -> Result<()> {
    init_with(connection, true)
}

/// Revoke the published demo credentials on a real database. Account names
/// and history are kept; only tokens that are public knowledge stop working.
fn disable_published_demo_credentials(connection: &Connection) -> Result<()> {
    for (id, token, _team) in DEMO_PRINCIPALS {
        connection
            .execute(
                "UPDATE principal_tokens SET revoked_at = unixepoch()
                 WHERE principal_id = ?1 AND revoked_at IS NULL
                   AND (label = ?2 OR (label = ?3 AND EXISTS (
                         SELECT 1 FROM principals WHERE id = ?1 AND token = ?4)))",
                params![id, SEED_LABEL, MIGRATED_LEGACY_LABEL, token],
            )
            .context("revoke published demo token")?;
        connection
            .execute(
                "UPDATE principals
                 SET token = '__hashed__:disabled-' || lower(hex(randomblob(16)))
                 WHERE id = ?1 AND token = ?2",
                params![id, token],
            )
            .context("disable published demo credential")?;
    }
    Ok(())
}

fn init_with(connection: &Connection, seed_demo: bool) -> Result<()> {
    connection
        .execute_batch(
            "
            PRAGMA journal_mode = WAL;

            CREATE TABLE IF NOT EXISTS principals (
              id TEXT PRIMARY KEY,
              nick TEXT NOT NULL,
              token TEXT UNIQUE NOT NULL,
              team TEXT,
              last_seen_seq INTEGER NOT NULL DEFAULT 0,
              connected_at INTEGER,
              disconnected_at INTEGER,
              created_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS channel_memberships (
              principal_id TEXT NOT NULL,
              channel TEXT NOT NULL,
              joined_at INTEGER NOT NULL,
              last_seen_seq INTEGER NOT NULL DEFAULT 0,
              PRIMARY KEY (principal_id, channel)
            );

            CREATE TABLE IF NOT EXISTS messages (
              seq INTEGER PRIMARY KEY AUTOINCREMENT,
              msg_id TEXT UNIQUE NOT NULL,
              channel TEXT NOT NULL,
              sender_id TEXT NOT NULL,
              body TEXT NOT NULL,
              created_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS tasks (
              id TEXT PRIMARY KEY,
              channel TEXT NOT NULL,
              title TEXT NOT NULL,
              status TEXT NOT NULL,
              claimed_by TEXT,
              lease_expires_at INTEGER,
              created_at INTEGER NOT NULL,
              updated_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS principal_tokens (
              id INTEGER PRIMARY KEY AUTOINCREMENT,
              principal_id TEXT NOT NULL REFERENCES principals(id) ON DELETE CASCADE,
              token_id TEXT UNIQUE NOT NULL,
              token_hash TEXT NOT NULL,
              label TEXT,
              created_at INTEGER NOT NULL,
              revoked_at INTEGER
            );

            CREATE INDEX IF NOT EXISTS idx_principal_tokens_principal
              ON principal_tokens (principal_id);

            -- Guarantees at most one backfill row per principal, even if init
            -- runs repeatedly against an already-populated database.
            CREATE UNIQUE INDEX IF NOT EXISTS idx_principal_tokens_principal_label
              ON principal_tokens (principal_id, label);

            CREATE TABLE IF NOT EXISTS webhooks (
              principal_id TEXT PRIMARY KEY REFERENCES principals(id) ON DELETE CASCADE,
              url TEXT NOT NULL,
              secret TEXT NOT NULL,
              events TEXT NOT NULL,
              notify_when TEXT NOT NULL DEFAULT 'always',
              enabled INTEGER NOT NULL DEFAULT 1,
              last_delivery_at INTEGER,
              last_status INTEGER,
              last_error TEXT,
              created_at INTEGER NOT NULL,
              updated_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS webhook_deliveries (
              id TEXT PRIMARY KEY,
              principal_id TEXT NOT NULL REFERENCES principals(id) ON DELETE CASCADE,
              event_type TEXT NOT NULL,
              channel TEXT NOT NULL,
              payload_json TEXT NOT NULL,
              status TEXT NOT NULL,
              attempts INTEGER NOT NULL DEFAULT 0,
              next_attempt_at INTEGER NOT NULL,
              http_status INTEGER,
              last_error TEXT,
              created_at INTEGER NOT NULL,
              updated_at INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_webhook_deliveries_due
              ON webhook_deliveries (status, next_attempt_at);

            CREATE INDEX IF NOT EXISTS idx_webhook_deliveries_principal
              ON webhook_deliveries (principal_id, created_at DESC);

            CREATE TABLE IF NOT EXISTS reports (
              id TEXT PRIMARY KEY,
              author_id TEXT NOT NULL REFERENCES principals(id) ON DELETE CASCADE,
              channel TEXT NOT NULL,
              title TEXT NOT NULL,
              status TEXT NOT NULL,
              summary TEXT NOT NULL,
              body TEXT NOT NULL,
              task_id TEXT,
              git_repo TEXT,
              git_branch TEXT,
              git_commit TEXT,
              created_at INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_reports_channel_created
              ON reports (channel, created_at DESC);

            CREATE TABLE IF NOT EXISTS git_repos (
              name TEXT PRIMARY KEY,
              channel TEXT NOT NULL,
              local_path TEXT NOT NULL,
              remote_url TEXT,
              default_branch TEXT,
              webhook_secret TEXT,
              last_commit TEXT,
              last_branch TEXT,
              last_event_at INTEGER,
              created_at INTEGER NOT NULL,
              updated_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS resource_locks (
              resource TEXT PRIMARY KEY,
              owner_id TEXT NOT NULL REFERENCES principals(id) ON DELETE CASCADE,
              channel TEXT NOT NULL,
              note TEXT,
              expires_at INTEGER NOT NULL,
              updated_at INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_resource_locks_channel
              ON resource_locks (channel, expires_at);

            CREATE TABLE IF NOT EXISTS projects (
              channel TEXT PRIMARY KEY,
              name TEXT NOT NULL,
              description TEXT,
              created_by TEXT NOT NULL,
              created_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS project_roles (
              channel TEXT NOT NULL,
              principal_id TEXT NOT NULL,
              role TEXT NOT NULL,
              updated_at INTEGER NOT NULL,
              PRIMARY KEY (channel, principal_id)
            );

            CREATE TABLE IF NOT EXISTS task_events (
              id INTEGER PRIMARY KEY AUTOINCREMENT,
              task_id TEXT NOT NULL,
              actor_id TEXT NOT NULL,
              action TEXT NOT NULL,
              from_status TEXT,
              to_status TEXT,
              note TEXT,
              created_at INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_task_events_task
              ON task_events (task_id, id);

            CREATE TABLE IF NOT EXISTS wake_configs (
              principal_id TEXT PRIMARY KEY,
              command TEXT NOT NULL,
              args_json TEXT NOT NULL,
              cwd TEXT,
              env_json TEXT NOT NULL,
              prompt TEXT,
              enabled INTEGER NOT NULL DEFAULT 1,
              cooldown_seconds INTEGER NOT NULL,
              timeout_seconds INTEGER NOT NULL,
              max_runs_per_hour INTEGER NOT NULL,
              updated_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS wake_runs (
              id TEXT PRIMARY KEY,
              principal_id TEXT NOT NULL,
              reason TEXT NOT NULL,
              channel TEXT NOT NULL,
              task_id TEXT,
              status TEXT NOT NULL,
              exit_code INTEGER,
              started_at INTEGER NOT NULL,
              finished_at INTEGER,
              log_tail TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_wake_runs_principal
              ON wake_runs (principal_id, started_at);

            CREATE TABLE IF NOT EXISTS wake_sessions (
              principal_id TEXT NOT NULL,
              scope_key TEXT NOT NULL,
              session_id TEXT NOT NULL,
              updated_at INTEGER NOT NULL,
              PRIMARY KEY (principal_id, scope_key)
            );
            ",
        )
        .context("initialize sqlite schema")?;

    migrate_principals(connection)?;
    migrate_channel_memberships(connection)?;
    migrate_tasks(connection)?;
    migrate_wake_configs(connection)?;

    if seed_demo {
        for (id, token, team) in DEMO_PRINCIPALS {
            seed_principal(connection, id, id, token, team)?;
        }
    } else {
        disable_published_demo_credentials(connection)?;
    }

    backfill_principal_tokens(connection)?;

    Ok(())
}

fn migrate_wake_configs(connection: &Connection) -> Result<()> {
    let columns = [
        ("session_mode", "TEXT NOT NULL DEFAULT 'fresh'"),
        ("resume_args_json", "TEXT NOT NULL DEFAULT '[]'"),
        ("session_marker", "TEXT"),
        ("session_scope", "TEXT NOT NULL DEFAULT 'account'"),
    ];

    for (column, definition) in columns {
        let exists: i64 = connection.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('wake_configs') WHERE name = ?1",
            params![column],
            |row| row.get(0),
        )?;

        if exists == 0 {
            connection.execute(
                &format!("ALTER TABLE wake_configs ADD COLUMN {column} {definition}"),
                [],
            )?;
        }
    }

    Ok(())
}

fn migrate_tasks(connection: &Connection) -> Result<()> {
    let columns = [
        ("body", "TEXT"),
        ("kind", "TEXT NOT NULL DEFAULT 'task'"),
        ("assignee", "TEXT"),
        ("created_by", "TEXT"),
        ("repo", "TEXT"),
        ("branch", "TEXT"),
        ("issue_url", "TEXT"),
    ];

    for (column, definition) in columns {
        let exists: i64 = connection.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('tasks') WHERE name = ?1",
            params![column],
            |row| row.get(0),
        )?;

        if exists == 0 {
            connection.execute(
                &format!("ALTER TABLE tasks ADD COLUMN {column} {definition}"),
                [],
            )?;
        }
    }

    connection.execute(
        "CREATE INDEX IF NOT EXISTS idx_tasks_assignee ON tasks (assignee, status)",
        [],
    )?;

    Ok(())
}

fn migrate_channel_memberships(connection: &Connection) -> Result<()> {
    let exists: i64 = connection.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('channel_memberships') WHERE name = 'last_seen_seq'",
        [],
        |row| row.get(0),
    )?;

    if exists == 0 {
        connection.execute(
            "ALTER TABLE channel_memberships ADD COLUMN last_seen_seq INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }

    Ok(())
}

fn seed_principal(
    connection: &Connection,
    id: &str,
    nick: &str,
    token: &str,
    team: &str,
) -> Result<()> {
    let inserted = connection
        .execute(
            "INSERT OR IGNORE INTO principals (id, nick, token, team, created_at)
             VALUES (?1, ?2, ?3, ?4, unixepoch())",
            params![id, nick, token, team],
        )
        .context("seed principal")?;

    // Only mirror the hardcoded demo token into `principal_tokens` when this
    // call actually created the principal row. If the principal already exists
    // its current `principals.token` may have been rotated away from the
    // hardcoded default, so hashing `token` here would write the wrong secret
    // and also block `backfill_principal_tokens` from recording the real
    // plaintext under the `migrated-legacy` label.
    if inserted == 0 {
        return Ok(());
    }

    let (_token_id, token_id_hex, _) = tokens::generate();
    let token_hash = tokens::hash(token).context("hash seed token")?;
    connection
        .execute(
            "INSERT INTO principal_tokens (principal_id, token_id, token_hash, label, created_at)
             VALUES (?1, ?2, ?3, ?4, unixepoch())",
            params![id, token_id_hex, token_hash, SEED_LABEL],
        )
        .context("seed principal_tokens row")?;

    Ok(())
}

fn migrate_principals(connection: &Connection) -> Result<()> {
    let columns = [
        ("last_seen_seq", "INTEGER NOT NULL DEFAULT 0"),
        ("connected_at", "INTEGER"),
        ("disconnected_at", "INTEGER"),
        ("role", "TEXT NOT NULL DEFAULT 'member'"),
    ];

    for (column, definition) in columns {
        let exists: i64 = connection.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('principals') WHERE name = ?1",
            params![column],
            |row| row.get(0),
        )?;

        if exists == 0 {
            connection.execute(
                &format!("ALTER TABLE principals ADD COLUMN {column} {definition}"),
                [],
            )?;
        }
    }

    Ok(())
}

/// Mirror every legacy `principals.token` row into `principal_tokens` as a
/// `migrated-legacy` entry, keyed by a fresh random `token_id` with the
/// existing plaintext token hashed as `token_hash`. Principals that already
/// have ANY `principal_tokens` row are skipped — this covers both restart
/// idempotency and freshly seeded demo principals whose `seed` row is inserted
/// by [`seed_principal`] before this function runs. The UNIQUE
/// `(principal_id, label)` index provides a second line of defence.
fn backfill_principal_tokens(connection: &Connection) -> Result<()> {
    let mut select = connection
        .prepare(
            "SELECT p.id, p.token
             FROM principals p
             LEFT JOIN principal_tokens t ON t.principal_id = p.id
             WHERE p.token IS NOT NULL AND t.id IS NULL",
        )
        .context("prepare principal_tokens backfill query")?;

    let rows = select
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("collect principals needing backfill")?;

    for (principal_id, plaintext) in rows {
        let (_, token_id_hex, _) = tokens::generate();
        let token_hash = tokens::hash(&plaintext).context("hash legacy token for backfill")?;
        connection
            .execute(
                "INSERT INTO principal_tokens (principal_id, token_id, token_hash, label, created_at)
                 VALUES (?1, ?2, ?3, ?4, unixepoch())",
                params![principal_id, token_id_hex, token_hash, MIGRATED_LEGACY_LABEL],
            )
            .context("insert migrated-legacy principal_tokens row")?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn published_demo_tokens_stop_working_on_a_real_database() -> Result<()> {
        let connection = Connection::open_in_memory()?;
        // A database created by an older build: demo principals are present.
        init_with(&connection, true)?;
        // Reopened by a locked-down build.
        init_with(&connection, false)?;
        init_with(&connection, false)?; // idempotent

        let shared: SharedConnection = Arc::new(Mutex::new(connection));
        let auth = crate::auth::AuthStore::new(shared.clone());
        for (_id, token, _team) in DEMO_PRINCIPALS {
            assert!(auth.authenticate(token).is_err(), "{token} must be rejected");
        }
        let kept: i64 = shared.lock().unwrap().query_row(
            "SELECT COUNT(*) FROM principals WHERE id = 'human'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(kept, 1, "account name and history are kept");
        Ok(())
    }

    #[test]
    fn init_creates_principal_tokens_table_and_seeds_mirrors() -> Result<()> {
        let connection = Connection::open_in_memory()?;
        init(&connection)?;

        let seed_count: i64 = connection.query_row(
            "SELECT COUNT(*) FROM principal_tokens WHERE label = ?1",
            params![SEED_LABEL],
            |row| row.get(0),
        )?;
        assert_eq!(seed_count, 7, "every seeded principal should have a row");

        let unique_ids: i64 = connection.query_row(
            "SELECT COUNT(DISTINCT token_id) FROM principal_tokens",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(unique_ids, seed_count);
        Ok(())
    }

    #[test]
    fn backfill_is_idempotent_across_reinit() -> Result<()> {
        let connection = Connection::open_in_memory()?;
        init(&connection)?;
        let first: i64 =
            connection.query_row("SELECT COUNT(*) FROM principal_tokens", [], |row| {
                row.get(0)
            })?;

        // Simulate a legacy principal inserted before principal_tokens existed.
        connection.execute(
            "INSERT INTO principals (id, nick, token, team, created_at)
             VALUES ('legacy', 'legacy', 'legacy-plaintext', 'ops', unixepoch())",
            [],
        )?;

        init(&connection)?;
        let second: i64 =
            connection.query_row("SELECT COUNT(*) FROM principal_tokens", [], |row| {
                row.get(0)
            })?;
        assert_eq!(second, first + 1, "one legacy row should be backfilled");

        init(&connection)?;
        let third: i64 =
            connection.query_row("SELECT COUNT(*) FROM principal_tokens", [], |row| {
                row.get(0)
            })?;
        assert_eq!(third, second, "backfill must not duplicate rows");

        let (label, hash): (String, String) = connection.query_row(
            "SELECT label, token_hash FROM principal_tokens WHERE principal_id = 'legacy'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(label, MIGRATED_LEGACY_LABEL);
        assert!(tokens::verify("legacy-plaintext", &hash)?);
        Ok(())
    }

    #[test]
    fn reinit_preserves_rotated_plaintext_via_backfill() -> Result<()> {
        // Simulate a pre-existing DB where a built-in principal's legacy token
        // was rotated away from the hardcoded seed default before
        // principal_tokens existed. `init` on such a DB must not silently
        // hash the stale hardcoded default — it must hash the actual current
        // `principals.token` via the migrated-legacy backfill.
        let connection = Connection::open_in_memory()?;
        connection.execute_batch(
            "CREATE TABLE principals (
               id TEXT PRIMARY KEY,
               nick TEXT NOT NULL,
               token TEXT UNIQUE NOT NULL,
               team TEXT,
               last_seen_seq INTEGER NOT NULL DEFAULT 0,
               connected_at INTEGER,
               disconnected_at INTEGER,
               created_at INTEGER NOT NULL
             );",
        )?;
        connection.execute(
            "INSERT INTO principals (id, nick, token, team, created_at)
             VALUES ('human', 'human', 'rotated-secret', 'demo', unixepoch())",
            [],
        )?;

        init(&connection)?;

        let (label, hash): (String, String) = connection.query_row(
            "SELECT label, token_hash FROM principal_tokens WHERE principal_id = 'human'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(
            label, MIGRATED_LEGACY_LABEL,
            "pre-existing principal with rotated token must take the migrated-legacy backfill path, not the seed path"
        );
        assert!(
            tokens::verify("rotated-secret", &hash)?,
            "backfill must hash the actual current plaintext, not the hardcoded default"
        );
        assert!(
            !tokens::verify("human-token", &hash)?,
            "the hardcoded seed default must not be written for a pre-existing principal"
        );
        Ok(())
    }

    #[test]
    fn init_creates_extended_tables_and_role_column() -> Result<()> {
        let connection = Connection::open_in_memory()?;
        init(&connection)?;

        for table in [
            "webhooks",
            "webhook_deliveries",
            "reports",
            "git_repos",
            "resource_locks",
        ] {
            let exists: i64 = connection.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                params![table],
                |row| row.get(0),
            )?;
            assert_eq!(exists, 1, "table {table} must exist after init");
        }

        let role: String = connection.query_row(
            "SELECT role FROM principals WHERE id = 'human'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(role, "member");
        Ok(())
    }
}
