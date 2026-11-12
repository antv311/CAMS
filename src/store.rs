use anyhow::{anyhow, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::history::unix_timestamp;
use crate::tokens;

pub type SharedConnection = Arc<Mutex<Connection>>;

/// An owner-defined client launcher. Placeholders such as `{model}` and
/// `{prompt}` are stored verbatim; the chosen model belongs to the worker,
/// not to the template. Executable availability is runtime state and is not
/// stored here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientTemplate {
    pub id: String,
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    /// As with `WakeConfig`, `None` removes a variable from the child environment.
    pub env: BTreeMap<String, Option<String>>,
    pub prompt: Option<String>,
    pub session_mode: String,
    pub resume_args: Vec<String>,
    pub session_marker: Option<String>,
    pub session_scope: String,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Full replacement input for a template. Missing session settings default
/// to a fresh conversation with account scope, matching the launcher.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ClientTemplateInput {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub env: BTreeMap<String, Option<String>>,
    pub prompt: Option<String>,
    pub session_mode: Option<String>,
    pub resume_args: Vec<String>,
    pub session_marker: Option<String>,
    pub session_scope: Option<String>,
}

/// Persistence only: callers enforce owner/PM permissions and provision accounts.
#[derive(Clone)]
pub struct ClientTemplateStore {
    db: SharedConnection,
}

const CLIENT_TEMPLATE_SELECT: &str =
    "SELECT id, name, command, args_json, cwd, env_json, prompt, session_mode,
            resume_args_json, session_marker, session_scope, created_at, updated_at
     FROM client_templates";

impl ClientTemplateStore {
    pub fn new(db: SharedConnection) -> Self {
        Self { db }
    }

    /// IDs are case-sensitive ASCII letters, digits, `-` or `_`, with outer
    /// whitespace trimmed. Create or replace the mutable fields of a template. Its original
    /// creation time is retained. No executable is invoked or model restricted.
    pub fn upsert(&self, id: &str, input: ClientTemplateInput) -> Result<ClientTemplate> {
        let id = client_template_id(id)?;
        let name = input.name.trim();
        let command = input.command.trim();
        if name.is_empty() || name.contains('\0') {
            return Err(anyhow!(
                "client template name is required and must not contain NUL"
            ));
        }
        if command.is_empty() || command.contains('\0') {
            return Err(anyhow!(
                "client template command is required and must not contain NUL"
            ));
        }
        let session_mode = template_setting(
            input.session_mode.as_deref(),
            &["fresh", "resume", "pm"],
            "session_mode",
        )?;
        let session_scope = template_setting(
            input.session_scope.as_deref(),
            &["account", "channel"],
            "session_scope",
        )?;
        let db = self.db.lock().expect("sqlite mutex poisoned");
        db.execute(
            "INSERT INTO client_templates
               (id, name, command, args_json, cwd, env_json, prompt, session_mode,
                resume_args_json, session_marker, session_scope, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?12)
             ON CONFLICT(id) DO UPDATE SET
               name = excluded.name, command = excluded.command,
               args_json = excluded.args_json, cwd = excluded.cwd,
               env_json = excluded.env_json, prompt = excluded.prompt,
               session_mode = excluded.session_mode,
               resume_args_json = excluded.resume_args_json,
               session_marker = excluded.session_marker,
               session_scope = excluded.session_scope, updated_at = excluded.updated_at",
            params![
                id,
                name,
                command,
                serde_json::to_string(&input.args)?,
                input
                    .cwd
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
                serde_json::to_string(&input.env)?,
                input.prompt.filter(|s| !s.trim().is_empty()),
                session_mode,
                serde_json::to_string(&input.resume_args)?,
                input.session_marker.filter(|s| !s.trim().is_empty()),
                session_scope,
                unix_timestamp(),
            ],
        )
        .context("save client template")?;
        Self::get_locked(&db, id)?.ok_or_else(|| anyhow!("client template {id} vanished"))
    }

    pub fn get(&self, id: &str) -> Result<Option<ClientTemplate>> {
        let id = client_template_id(id)?;
        let db = self.db.lock().expect("sqlite mutex poisoned");
        Self::get_locked(&db, id)
    }

    /// Stable directory order, independent of insertion or update order.
    pub fn list(&self) -> Result<Vec<ClientTemplate>> {
        let db = self.db.lock().expect("sqlite mutex poisoned");
        let mut statement = db
            .prepare(&format!("{CLIENT_TEMPLATE_SELECT} ORDER BY id ASC"))
            .context("prepare client template list")?;
        let rows = statement
            .query_map([], Self::from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("read client templates")?;
        Ok(rows)
    }

    /// Returns false when no template exists. Existing workers are untouched.
    pub fn delete(&self, id: &str) -> Result<bool> {
        let id = client_template_id(id)?;
        let db = self.db.lock().expect("sqlite mutex poisoned");
        let changed = db
            .execute("DELETE FROM client_templates WHERE id = ?1", params![id])
            .context("delete client template")?;
        Ok(changed > 0)
    }

    fn get_locked(db: &Connection, id: &str) -> Result<Option<ClientTemplate>> {
        db.query_row(
            &format!("{CLIENT_TEMPLATE_SELECT} WHERE id = ?1"),
            params![id],
            Self::from_row,
        )
        .optional()
        .context("load client template")
    }

    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ClientTemplate> {
        fn json<T: serde::de::DeserializeOwned>(
            row: &rusqlite::Row<'_>,
            index: usize,
        ) -> rusqlite::Result<T> {
            let value: String = row.get(index)?;
            serde_json::from_str(&value).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    index,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })
        }

        Ok(ClientTemplate {
            id: row.get(0)?,
            name: row.get(1)?,
            command: row.get(2)?,
            args: json(row, 3)?,
            cwd: row.get(4)?,
            env: json(row, 5)?,
            prompt: row.get(6)?,
            session_mode: row.get(7)?,
            resume_args: json(row, 8)?,
            session_marker: row.get(9)?,
            session_scope: row.get(10)?,
            created_at: row.get(11)?,
            updated_at: row.get(12)?,
        })
    }
}

fn client_template_id(id: &str) -> Result<&str> {
    let id = id.trim();
    if id.is_empty()
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return Err(anyhow!(
            "client template id must contain only ASCII letters, digits, '-' or '_'"
        ));
    }
    Ok(id)
}

fn template_setting(value: Option<&str>, allowed: &[&str], name: &str) -> Result<String> {
    let value = value.unwrap_or(allowed[0]).trim().to_ascii_lowercase();
    let value = if value.is_empty() { allowed[0] } else { &value };
    if !allowed.contains(&value) {
        return Err(anyhow!("{name} must be one of: {}", allowed.join(", ")));
    }
    Ok(value.to_string())
}

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

            CREATE TABLE IF NOT EXISTS rooms (
              channel TEXT PRIMARY KEY,
              project TEXT NOT NULL,
              label TEXT NOT NULL,
              title TEXT,
              opened_by TEXT NOT NULL,
              opened_at INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_rooms_project ON rooms (project, opened_at);

            CREATE TABLE IF NOT EXISTS task_questions (
              id TEXT PRIMARY KEY,
              task_id TEXT NOT NULL,
              channel TEXT NOT NULL,
              asked_by TEXT NOT NULL,
              question TEXT NOT NULL,
              status TEXT NOT NULL,
              resume_status TEXT NOT NULL,
              answer TEXT,
              answered_by TEXT,
              escalation_note TEXT,
              created_at INTEGER NOT NULL,
              answered_at INTEGER
            );

            CREATE INDEX IF NOT EXISTS idx_task_questions_status
              ON task_questions (status, channel);

            CREATE TABLE IF NOT EXISTS account_notes (
              principal_id TEXT PRIMARY KEY,
              body TEXT NOT NULL,
              updated_at INTEGER NOT NULL
            );

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
    migrate_projects(connection)?;
    migrate_client_templates(connection)?;

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

/// Additive and idempotent for databases created before client templates.
fn migrate_client_templates(connection: &Connection) -> Result<()> {
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS client_templates (
           id TEXT PRIMARY KEY NOT NULL,
           name TEXT NOT NULL,
           command TEXT NOT NULL,
           args_json TEXT NOT NULL DEFAULT '[]',
           cwd TEXT,
           env_json TEXT NOT NULL DEFAULT '{}',
           prompt TEXT,
           session_mode TEXT NOT NULL DEFAULT 'fresh'
             CHECK (session_mode IN ('fresh', 'resume', 'pm')),
           resume_args_json TEXT NOT NULL DEFAULT '[]',
           session_marker TEXT,
           session_scope TEXT NOT NULL DEFAULT 'account'
             CHECK (session_scope IN ('account', 'channel')),
           created_at INTEGER NOT NULL,
           updated_at INTEGER NOT NULL
         );",
        )
        .context("migrate client templates")?;
    Ok(())
}

fn migrate_projects(connection: &Connection) -> Result<()> {
    let exists: i64 = connection.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('projects') WHERE name = 'pm'",
        [],
        |row| row.get(0),
    )?;
    if exists == 0 {
        connection.execute("ALTER TABLE projects ADD COLUMN pm TEXT", [])?;
    }
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
        ("sprint", "TEXT"),
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

    fn template_input() -> ClientTemplateInput {
        ClientTemplateInput {
            name: "Example Client".to_string(),
            command: "example-client".to_string(),
            args: vec![
                "--model".to_string(),
                "{model}".to_string(),
                "{prompt}".to_string(),
            ],
            ..Default::default()
        }
    }

    fn template_store() -> Result<ClientTemplateStore> {
        let connection = Connection::open_in_memory()?;
        init_with(&connection, false)?;
        Ok(ClientTemplateStore::new(Arc::new(Mutex::new(connection))))
    }

    #[test]
    fn client_templates_round_trip_launcher_settings_verbatim() -> Result<()> {
        let store = template_store()?;
        let input = ClientTemplateInput {
            name: "Example \"Client\" — 日本語".to_string(),
            command: r"C:\Agent Tools\example.exe".to_string(),
            args: vec![
                "--model={model}".to_string(),
                "{prompt}".to_string(),
                "".to_string(),
                "O'Brien says \"hello\"\n$literal `text`".to_string(),
            ],
            cwd: Some("{repo_path}".to_string()),
            env: BTreeMap::from([
                (
                    "KEEP".to_string(),
                    Some("quote=\"; newline\n{model}".to_string()),
                ),
                ("EMPTY".to_string(), Some(String::new())),
                ("REMOVE".to_string(), None),
            ]),
            prompt: Some("Start\n{nick}: {reason}".to_string()),
            session_mode: Some("resume".to_string()),
            resume_args: vec![
                "resume".to_string(),
                "{session_id}".to_string(),
                "{model}".to_string(),
            ],
            session_marker: Some("\"session_id\":".to_string()),
            session_scope: Some("channel".to_string()),
        };
        let saved = store.upsert("example", input.clone())?;
        assert_eq!(saved.name, input.name);
        assert_eq!(saved.command, input.command);
        assert_eq!(saved.args, input.args);
        assert_eq!(saved.cwd, input.cwd);
        assert_eq!(saved.env, input.env);
        assert_eq!(saved.prompt, input.prompt);
        assert_eq!(saved.session_mode, "resume");
        assert_eq!(saved.resume_args, input.resume_args);
        assert_eq!(saved.session_marker, input.session_marker);
        assert_eq!(saved.session_scope, "channel");
        assert!(saved.created_at > 0);
        assert_eq!(saved.created_at, saved.updated_at);
        assert_eq!(store.get("example")?, Some(saved.clone()));
        assert_eq!(store.list()?, vec![saved]);
        Ok(())
    }

    #[test]
    fn client_templates_defaults_and_optional_values_match_launcher() -> Result<()> {
        let store = template_store()?;
        let input = serde_json::from_value(serde_json::json!({
            "name": "  Example Client  ", "command": "  example-client  ",
            "cwd": "  ", "prompt": "\n", "session_marker": " ",
            "session_mode": " ", "session_scope": " "
        }))?;
        let saved = store.upsert(" example ", input)?;
        assert_eq!(saved.id, "example");
        assert_eq!(saved.name, "Example Client");
        assert_eq!(saved.command, "example-client");
        assert_eq!(saved.session_mode, "fresh");
        assert_eq!(saved.session_scope, "account");
        assert!(saved.args.is_empty());
        assert!(saved.resume_args.is_empty());
        assert!(saved.env.is_empty());
        assert_eq!(saved.cwd, None);
        assert_eq!(saved.prompt, None);
        assert_eq!(saved.session_marker, None);
        assert_eq!(store.get(" example ")?, Some(saved));
        let defaulted = store.upsert("minimal", template_input())?;
        assert_eq!(defaulted.session_mode, "fresh");
        assert_eq!(defaulted.session_scope, "account");
        Ok(())
    }

    #[test]
    fn client_templates_update_replaces_fields_and_keeps_creation_time() -> Result<()> {
        let store = template_store()?;
        let mut input = template_input();
        input.env.insert("REMOVE".to_string(), None);
        input.resume_args.push("{session_id}".to_string());
        input.session_mode = Some("resume".to_string());
        store.upsert("example", input)?;
        store.db.lock().unwrap().execute(
            "UPDATE client_templates SET created_at = 1, updated_at = 2 WHERE id = 'example'",
            [],
        )?;
        let mut replacement = template_input();
        replacement.name = "Renamed".to_string();
        replacement.command = "new-client".to_string();
        replacement.args.clear();
        let updated = store.upsert("example", replacement)?;
        assert_eq!(updated.created_at, 1);
        assert!(updated.updated_at > 2);
        assert_eq!(updated.name, "Renamed");
        assert_eq!(updated.command, "new-client");
        assert!(updated.env.is_empty());
        assert!(updated.args.is_empty());
        assert!(updated.resume_args.is_empty());
        assert_eq!(updated.session_mode, "fresh");
        assert_eq!(store.list()?.len(), 1);
        assert_eq!(store.get("example")?, Some(updated));
        Ok(())
    }

    #[test]
    fn client_templates_directory_and_delete_are_deterministic() -> Result<()> {
        let store = template_store()?;
        assert!(store.list()?.is_empty());
        assert_eq!(store.get("missing")?, None);
        assert!(!store.delete("missing")?);
        for id in ["zeta", "alpha", "middle"] {
            store.upsert(id, template_input())?;
        }
        let ids: Vec<_> = store.list()?.into_iter().map(|row| row.id).collect();
        assert_eq!(ids, ["alpha", "middle", "zeta"]);
        assert!(store.delete(" middle ")?);
        assert!(!store.delete("middle")?);
        assert_eq!(store.get("middle")?, None);
        assert_eq!(store.list()?.len(), 2);
        Ok(())
    }

    #[test]
    fn client_templates_reject_invalid_input_without_mutation() -> Result<()> {
        let store = template_store()?;
        let original = store.upsert("example", template_input())?;
        for id in ["", " ", "two words", "../example", "example\0", "é"] {
            assert!(store.upsert(id, template_input()).is_err(), "{id:?}");
            assert!(store.get(id).is_err(), "{id:?}");
            assert!(store.delete(id).is_err(), "{id:?}");
        }
        for (field, value) in [
            ("name", " "),
            ("command", "\n"),
            ("name", "nul\0"),
            ("command", "nul\0"),
            ("session_mode", "unknown"),
            ("session_scope", "unknown"),
        ] {
            let mut input = template_input();
            match field {
                "name" => input.name = value.to_string(),
                "command" => input.command = value.to_string(),
                "session_mode" => input.session_mode = Some(value.to_string()),
                "session_scope" => input.session_scope = Some(value.to_string()),
                _ => unreachable!(),
            }
            assert!(store.upsert("example", input).is_err(), "{field}");
            assert_eq!(store.get("example")?, Some(original.clone()));
        }
        assert_eq!(store.list()?, vec![original]);
        Ok(())
    }

    #[test]
    fn client_templates_support_all_launcher_session_modes() -> Result<()> {
        let store = template_store()?;
        for (mode, scope) in [
            ("fresh", "account"),
            ("resume", "channel"),
            ("pm", "channel"),
        ] {
            let mut input = template_input();
            input.session_mode = Some(format!(" {mode} ").to_uppercase());
            input.session_scope = Some(format!(" {scope} ").to_uppercase());
            let saved = store.upsert("example", input)?;
            assert_eq!(saved.session_mode, mode);
            assert_eq!(saved.session_scope, scope);
        }
        Ok(())
    }

    #[test]
    fn client_templates_migrate_existing_database_without_changing_launchers() -> Result<()> {
        let connection = Connection::open_in_memory()?;
        // A database from before template storage and session-aware launchers.
        connection.execute_batch(
            "CREATE TABLE wake_configs (
               principal_id TEXT PRIMARY KEY, command TEXT NOT NULL,
               args_json TEXT NOT NULL, cwd TEXT, env_json TEXT NOT NULL, prompt TEXT,
               enabled INTEGER NOT NULL DEFAULT 1, cooldown_seconds INTEGER NOT NULL,
               timeout_seconds INTEGER NOT NULL, max_runs_per_hour INTEGER NOT NULL,
               updated_at INTEGER NOT NULL
             );
             INSERT INTO wake_configs VALUES
               ('existing-worker', 'existing-command', '[\"{prompt}\"]', NULL, '{}',
                NULL, 1, 20, 1800, 12, 123);",
        )?;
        init_with(&connection, false)?;
        let store = ClientTemplateStore::new(Arc::new(Mutex::new(connection)));
        assert!(
            store.list()?.is_empty(),
            "templates are defined by the owner"
        );
        let saved = store.upsert("example", template_input())?;
        {
            let db = store.db.lock().unwrap();
            init_with(&db, false)?;
            init_with(&db, false)?;
        }
        assert_eq!(store.get("example")?, Some(saved));
        let wake = crate::wake::WakeManager::new(store.db.clone());
        let existing = wake.get("existing-worker")?.unwrap();
        assert_eq!(existing.command, "existing-command");
        assert_eq!(existing.args, ["{prompt}"]);
        assert_eq!(existing.updated_at, 123);
        assert_eq!(existing.session_mode, "fresh");
        assert_eq!(existing.session_scope, "account");
        assert!(store.delete("example")?);
        assert!(wake.get("existing-worker")?.is_some());
        Ok(())
    }

    #[test]
    fn client_templates_survive_database_reopen() -> Result<()> {
        struct TestDb(std::path::PathBuf);
        impl Drop for TestDb {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let file = TestDb(std::env::temp_dir().join(format!(
            "aircd-client-templates-{}.sqlite3",
            uuid::Uuid::new_v4()
        )));
        let saved = {
            let store = ClientTemplateStore::new(open(&file.0)?);
            store.upsert("example", template_input())?
        };
        {
            let store = ClientTemplateStore::new(open(&file.0)?);
            assert_eq!(store.get("example")?, Some(saved));
            assert!(store.delete("example")?);
        }
        let store = ClientTemplateStore::new(open(&file.0)?);
        assert!(store.list()?.is_empty());
        Ok(())
    }

    #[test]
    fn client_templates_report_corrupt_json_instead_of_losing_settings() -> Result<()> {
        let store = template_store()?;
        for (column, value) in [
            ("args_json", "invalid"),
            ("env_json", "[]"),
            ("resume_args_json", "{}"),
        ] {
            store.upsert("example", template_input())?;
            store.db.lock().unwrap().execute(
                &format!("UPDATE client_templates SET {column} = ?1 WHERE id = 'example'"),
                params![value],
            )?;
            assert!(store.get("example").is_err(), "{column}");
            assert!(store.list().is_err(), "{column}");
        }
        Ok(())
    }

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
            assert!(
                auth.authenticate(token).is_err(),
                "{token} must be rejected"
            );
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
