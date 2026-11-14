//! Task attachments: test logs and other evidence files kept with a task.
//!
//! A builder attaches the output of the tests they ran, so a reviewer can read
//! the evidence on the hub instead of trusting a one-line note.

use anyhow::{anyhow, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::history::unix_timestamp;
use crate::store::SharedConnection;

/// Largest single attachment. Logs bigger than this should be trimmed.
pub const MAX_ATTACHMENT_BYTES: usize = 4 * 1024 * 1024;
/// Total stored for one task, so a runaway loop cannot fill the database.
pub const MAX_TASK_ATTACHMENT_BYTES: i64 = 32 * 1024 * 1024;
/// Most attachments one task may hold.
pub const MAX_TASK_ATTACHMENTS: i64 = 100;
const MAX_NAME_CHARS: usize = 200;
const MAX_NOTE_CHARS: usize = 1_000;

/// What an attachment is for. `log` is test or build output; `evidence` is
/// anything else that backs a claim (a screenshot, a report, a diff).
pub const ATTACHMENT_KINDS: &[&str] = &["log", "evidence"];

/// Attachment metadata; the bytes are fetched separately.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct TaskAttachment {
    pub id: String,
    pub task_id: String,
    pub filename: String,
    pub content_type: String,
    pub kind: String,
    pub size: i64,
    pub sha256: String,
    pub note: Option<String>,
    pub uploaded_by: String,
    pub created_at: i64,
}

pub struct NewAttachment<'a> {
    pub task_id: &'a str,
    pub filename: &'a str,
    pub content_type: Option<&'a str>,
    pub kind: Option<&'a str>,
    pub note: Option<&'a str>,
    pub uploaded_by: &'a str,
    pub data: &'a [u8],
}

pub fn migrate(connection: &Connection) -> Result<()> {
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS task_attachments (
               id TEXT PRIMARY KEY NOT NULL,
               task_id TEXT NOT NULL,
               filename TEXT NOT NULL,
               content_type TEXT NOT NULL,
               kind TEXT NOT NULL,
               size INTEGER NOT NULL,
               sha256 TEXT NOT NULL,
               note TEXT,
               uploaded_by TEXT NOT NULL,
               created_at INTEGER NOT NULL,
               data BLOB NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_task_attachments_task
               ON task_attachments (task_id, created_at);",
        )
        .context("migrate task attachments")
}

/// Keep only the final path component and drop control characters, so a
/// name is safe to show and to use as a download filename.
pub fn sanitize_filename(raw: &str) -> Result<String> {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or_default();
    let cleaned: String = base
        .chars()
        .filter(|c| !c.is_control() && *c != '"')
        .take(MAX_NAME_CHARS)
        .collect();
    let cleaned = cleaned.trim().trim_start_matches('.').trim().to_string();
    if cleaned.is_empty() {
        return Err(anyhow!("an attachment needs a file name"));
    }
    Ok(cleaned)
}

/// A type from the file extension when the uploader sent none.
pub fn guess_content_type(filename: &str) -> &'static str {
    let ext = filename
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "log" | "txt" | "out" | "err" | "md" | "diff" | "patch" | "rs" | "py" | "toml" | "yaml"
        | "yml" | "csv" => "text/plain; charset=utf-8",
        "json" | "jsonl" => "application/json",
        "xml" | "junit" => "application/xml",
        "html" | "htm" => "text/html; charset=utf-8",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "gz" | "tgz" => "application/gzip",
        _ => "application/octet-stream",
    }
}

#[derive(Clone)]
pub struct AttachmentStore {
    db: SharedConnection,
}

impl AttachmentStore {
    pub fn new(db: SharedConnection) -> Self {
        Self { db }
    }

    pub fn add(&self, new: NewAttachment<'_>) -> Result<TaskAttachment> {
        if new.data.is_empty() {
            return Err(anyhow!("the attachment is empty"));
        }
        if new.data.len() > MAX_ATTACHMENT_BYTES {
            return Err(anyhow!(
                "the attachment is {} bytes; the limit is {MAX_ATTACHMENT_BYTES} bytes - trim the log first",
                new.data.len()
            ));
        }
        let filename = sanitize_filename(new.filename)?;
        let kind = match new.kind.map(str::trim).filter(|k| !k.is_empty()) {
            None => "log".to_string(),
            Some(kind) => {
                let kind = kind.to_ascii_lowercase();
                if !ATTACHMENT_KINDS.contains(&kind.as_str()) {
                    return Err(anyhow!(
                        "unknown attachment kind '{kind}'; use one of: {}",
                        ATTACHMENT_KINDS.join(", ")
                    ));
                }
                kind
            }
        };
        let content_type = new
            .content_type
            .map(str::trim)
            .filter(|t| !t.is_empty() && !t.eq_ignore_ascii_case("application/octet-stream"))
            .map(str::to_string)
            .unwrap_or_else(|| guess_content_type(&filename).to_string());
        let note = new
            .note
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(|n| n.chars().take(MAX_NOTE_CHARS).collect::<String>());
        let size = new.data.len() as i64;
        let sha256 = Sha256::digest(new.data)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let attachment = TaskAttachment {
            id: format!("att_{}", Uuid::new_v4().simple()),
            task_id: new.task_id.to_string(),
            filename,
            content_type,
            kind,
            size,
            sha256,
            note,
            uploaded_by: new.uploaded_by.to_string(),
            created_at: unix_timestamp(),
        };

        let mut db = self.db.write()?;
        let tx = db.transaction().context("begin attachment insert")?;
        let (count, total): (i64, i64) = tx.query_row(
            "SELECT COUNT(*), COALESCE(SUM(size), 0) FROM task_attachments WHERE task_id = ?1",
            params![attachment.task_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if count >= MAX_TASK_ATTACHMENTS {
            return Err(anyhow!(
                "task {} already has {MAX_TASK_ATTACHMENTS} attachments",
                attachment.task_id
            ));
        }
        if total + size > MAX_TASK_ATTACHMENT_BYTES {
            return Err(anyhow!(
                "task {} would hold more than {MAX_TASK_ATTACHMENT_BYTES} bytes of attachments",
                attachment.task_id
            ));
        }
        tx.execute(
            "INSERT INTO task_attachments
               (id, task_id, filename, content_type, kind, size, sha256, note, uploaded_by,
                created_at, data)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                attachment.id,
                attachment.task_id,
                attachment.filename,
                attachment.content_type,
                attachment.kind,
                attachment.size,
                attachment.sha256,
                attachment.note,
                attachment.uploaded_by,
                attachment.created_at,
                new.data,
            ],
        )
        .context("save task attachment")?;
        tx.execute(
            "INSERT INTO task_events (task_id, actor_id, action, from_status, to_status, note, created_at)
             VALUES (?1, ?2, 'attach', NULL, NULL, ?3, ?4)",
            params![
                attachment.task_id,
                attachment.uploaded_by,
                format!(
                    "{} {} ({} bytes, {})",
                    attachment.kind, attachment.filename, attachment.size, attachment.id
                ),
                attachment.created_at,
            ],
        )
        .context("record attach event")?;
        tx.commit().context("commit task attachment")?;
        Ok(attachment)
    }

    pub fn list(&self, task_id: &str) -> Result<Vec<TaskAttachment>> {
        let db = self.db.read()?;
        let mut statement = db
            .prepare(
                "SELECT id, task_id, filename, content_type, kind, size, sha256, note,
                        uploaded_by, created_at
                 FROM task_attachments WHERE task_id = ?1 ORDER BY created_at ASC, rowid ASC",
            )
            .context("prepare task attachments")?;
        let rows = statement
            .query_map(params![task_id], Self::from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Metadata and bytes of one attachment of `task_id`.
    pub fn get(&self, task_id: &str, id: &str) -> Result<Option<(TaskAttachment, Vec<u8>)>> {
        let db = self.db.read()?;
        db.query_row(
            "SELECT id, task_id, filename, content_type, kind, size, sha256, note,
                    uploaded_by, created_at, data
             FROM task_attachments WHERE task_id = ?1 AND id = ?2",
            params![task_id, id],
            |row| Ok((Self::from_row(row)?, row.get::<_, Vec<u8>>(10)?)),
        )
        .optional()
        .context("load task attachment")
    }

    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskAttachment> {
        Ok(TaskAttachment {
            id: row.get(0)?,
            task_id: row.get(1)?,
            filename: row.get(2)?,
            content_type: row.get(3)?,
            kind: row.get(4)?,
            size: row.get(5)?,
            sha256: row.get(6)?,
            note: row.get(7)?,
            uploaded_by: row.get(8)?,
            created_at: row.get(9)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store;

    fn new<'a>(data: &'a [u8], name: &'a str) -> NewAttachment<'a> {
        NewAttachment {
            task_id: "task_1",
            filename: name,
            content_type: None,
            kind: None,
            note: Some("  cargo test output "),
            uploaded_by: "worker",
            data,
        }
    }

    #[test]
    fn add_list_get_round_trip_and_record_an_event() -> Result<()> {
        let db = store::open(":memory:")?;
        let store = AttachmentStore::new(db.clone());
        let saved = store.add(new(b"test result: ok", "../../etc/cargo-test.log"))?;
        assert_eq!(saved.filename, "cargo-test.log");
        assert_eq!(saved.kind, "log");
        assert_eq!(saved.content_type, "text/plain; charset=utf-8");
        assert_eq!(saved.size, 15);
        assert_eq!(saved.note.as_deref(), Some("cargo test output"));
        assert_eq!(
            saved.sha256,
            format!("{:x}", Sha256::digest(b"test result: ok"))
        );
        assert_eq!(store.list("task_1")?, vec![saved.clone()]);
        assert!(store.list("task_2")?.is_empty());
        let (meta, data) = store.get("task_1", &saved.id)?.unwrap();
        assert_eq!(meta, saved);
        assert_eq!(data, b"test result: ok");
        assert!(store.get("task_2", &saved.id)?.is_none());
        let events: i64 = db.read()?.query_row(
            "SELECT COUNT(*) FROM task_events WHERE task_id = 'task_1' AND action = 'attach'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(events, 1);
        Ok(())
    }

    #[test]
    fn rejects_empty_oversized_unnamed_and_unknown_kinds() -> Result<()> {
        let store = AttachmentStore::new(store::open(":memory:")?);
        assert!(store.add(new(b"", "a.log")).is_err());
        let big = vec![b'x'; MAX_ATTACHMENT_BYTES + 1];
        assert!(store.add(new(&big, "a.log")).is_err());
        assert!(store.add(new(b"x", " / ")).is_err());
        assert!(store.add(new(b"x", "...")).is_err());
        let mut bad_kind = new(b"x", "a.log");
        bad_kind.kind = Some("binary");
        assert!(store.add(bad_kind).is_err());
        let mut evidence = new(b"x", "shot.png");
        evidence.kind = Some("Evidence");
        assert_eq!(store.add(evidence)?.content_type, "image/png");
        assert_eq!(store.list("task_1")?.len(), 1);
        Ok(())
    }

    #[test]
    fn enforces_per_task_total_size() -> Result<()> {
        let store = AttachmentStore::new(store::open(":memory:")?);
        let chunk = vec![b'x'; MAX_ATTACHMENT_BYTES];
        let fits = (MAX_TASK_ATTACHMENT_BYTES as usize) / MAX_ATTACHMENT_BYTES;
        for _ in 0..fits {
            store.add(new(&chunk, "a.log"))?;
        }
        let err = store.add(new(b"x", "a.log")).unwrap_err().to_string();
        assert!(err.contains("more than"), "{err}");
        Ok(())
    }
}
