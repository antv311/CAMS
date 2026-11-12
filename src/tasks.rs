use anyhow::{anyhow, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

use crate::history::unix_timestamp;

/// Statuses after which a task no longer appears in lists or inboxes.
pub const TERMINAL_STATUSES: &[&str] = &["done", "merged", "cancelled"];

/// Every status a task may hold. `open`/`claimed`/`done` are the original
/// lease-based flow; the rest model assigned work that goes through review.
pub const ALL_STATUSES: &[&str] = &[
    "open",
    "claimed",
    "assigned",
    "in_progress",
    "review",
    "changes_requested",
    "merged",
    "done",
    "cancelled",
];

const TASK_COLUMNS: &str = "id, channel, title, status, claimed_by, lease_expires_at, \
     body, kind, assignee, created_by, repo, branch, issue_url, created_at, updated_at";

#[derive(Clone, Debug, Serialize)]
pub struct Task {
    pub id: String,
    pub channel: String,
    pub title: String,
    pub status: String,
    pub claimed_by: Option<String>,
    pub lease_expires_at: Option<i64>,
    /// Full description: everything a new assignee needs without reading chat.
    pub body: Option<String>,
    /// `task`, `bug` or `feature`.
    pub kind: String,
    pub assignee: Option<String>,
    pub created_by: Option<String>,
    pub repo: Option<String>,
    pub branch: Option<String>,
    pub issue_url: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug, Default)]
pub struct NewTask {
    pub channel: String,
    pub title: String,
    pub body: Option<String>,
    pub kind: Option<String>,
    pub assignee: Option<String>,
    pub created_by: Option<String>,
    pub repo: Option<String>,
    pub branch: Option<String>,
    pub issue_url: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct TaskEvent {
    pub task_id: String,
    pub actor_id: String,
    pub action: String,
    pub from_status: Option<String>,
    pub to_status: Option<String>,
    pub note: Option<String>,
    pub created_at: i64,
}

#[derive(Clone)]
pub struct TaskStore {
    db: Arc<Mutex<Connection>>,
    lease_seconds: i64,
}

pub fn normalize_kind(raw: Option<&str>) -> Result<String> {
    let clean = raw.unwrap_or("task").trim().to_ascii_lowercase();
    match clean.as_str() {
        "" | "task" => Ok("task".to_string()),
        "bug" => Ok("bug".to_string()),
        "feature" | "feature-request" | "feature_request" => Ok("feature".to_string()),
        other => Err(anyhow!(
            "task kind must be task, bug or feature (got '{other}')"
        )),
    }
}

fn clean_opt(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

impl TaskStore {
    pub fn new(db: Arc<Mutex<Connection>>, lease_seconds: i64) -> Self {
        Self { db, lease_seconds }
    }

    pub fn create(&self, channel: &str, title: &str) -> Result<Task> {
        self.create_full(NewTask {
            channel: channel.to_string(),
            title: title.to_string(),
            ..NewTask::default()
        })
    }

    pub fn create_full(&self, new: NewTask) -> Result<Task> {
        let now = unix_timestamp();
        let id = format!("task_{}", Uuid::new_v4().simple());
        let kind = normalize_kind(new.kind.as_deref())?;
        let assignee = clean_opt(new.assignee);
        let status = if assignee.is_some() { "assigned" } else { "open" };

        let db = self.db.lock().expect("sqlite mutex poisoned");
        db.execute(
            "INSERT INTO tasks (id, channel, title, status, claimed_by, lease_expires_at,
                                body, kind, assignee, created_by, repo, branch, issue_url,
                                created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, NULL, NULL, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?12)",
            params![
                id,
                new.channel,
                new.title,
                status,
                clean_opt(new.body),
                kind,
                assignee,
                clean_opt(new.created_by),
                clean_opt(new.repo),
                clean_opt(new.branch),
                clean_opt(new.issue_url),
                now
            ],
        )
        .context("insert task")?;

        Self::get_locked(&db, &id)
    }

    pub fn get(&self, task_id: &str) -> Result<Option<Task>> {
        let db = self.db.lock().expect("sqlite mutex poisoned");
        db.query_row(
            &format!("SELECT {TASK_COLUMNS} FROM tasks WHERE id = ?1"),
            params![task_id],
            Self::from_row,
        )
        .optional()
        .context("load task")
    }

    pub fn claim(&self, task_id: &str, principal_id: &str) -> Result<Task> {
        let now = unix_timestamp();
        let lease_expires_at = now + self.lease_seconds;
        let db = self.db.lock().expect("sqlite mutex poisoned");

        // Only the lease-based flow is claimable; assigned work has an owner.
        let changed = db
            .execute(
                "UPDATE tasks
                 SET status = 'claimed', claimed_by = ?1, lease_expires_at = ?2, updated_at = ?3
                 WHERE id = ?4
                   AND status IN ('open', 'claimed')
                   AND (
                     status = 'open'
                     OR lease_expires_at IS NULL
                     OR lease_expires_at < ?3
                   )",
                params![principal_id, lease_expires_at, now, task_id],
            )
            .context("claim task")?;

        if changed != 1 {
            return Err(anyhow!("task is already claimed or done"));
        }

        Self::get_locked(&db, task_id)
    }

    pub fn done(&self, task_id: &str, principal_id: &str) -> Result<Task> {
        let now = unix_timestamp();
        let db = self.db.lock().expect("sqlite mutex poisoned");

        let changed = db
            .execute(
                "UPDATE tasks
                 SET status = 'done', updated_at = ?1
                 WHERE id = ?2 AND claimed_by = ?3 AND status = 'claimed'",
                params![now, task_id, principal_id],
            )
            .context("mark task done")?;

        if changed != 1 {
            return Err(anyhow!("task is not claimed by this principal"));
        }

        Self::get_locked(&db, task_id)
    }

    pub fn release(&self, task_id: &str, principal_id: &str) -> Result<Task> {
        let now = unix_timestamp();
        let db = self.db.lock().expect("sqlite mutex poisoned");

        let changed = db
            .execute(
                "UPDATE tasks
                 SET status = 'open', claimed_by = NULL, lease_expires_at = NULL, updated_at = ?1
                 WHERE id = ?2 AND claimed_by = ?3 AND status = 'claimed'",
                params![now, task_id, principal_id],
            )
            .context("release task")?;

        if changed != 1 {
            return Err(anyhow!("task is not claimed by this principal"));
        }

        Self::get_locked(&db, task_id)
    }

    /// Give the task to `assignee`, or back to the pool when `None`. Work that
    /// had not reached review restarts as `assigned`; a task in review or with
    /// changes requested keeps that status so the new owner sees where it is.
    pub fn assign(&self, task_id: &str, assignee: Option<&str>, actor_id: &str) -> Result<Task> {
        let now = unix_timestamp();
        let db = self.db.lock().expect("sqlite mutex poisoned");
        let current = Self::get_locked(&db, task_id)?;
        if TERMINAL_STATUSES.contains(&current.status.as_str()) {
            return Err(anyhow!("task is {} and cannot be reassigned", current.status));
        }
        let next_status = match (assignee, current.status.as_str()) {
            (None, _) => "open",
            (Some(_), "review") => "review",
            (Some(_), "changes_requested") => "changes_requested",
            (Some(_), _) => "assigned",
        };
        db.execute(
            "UPDATE tasks
             SET assignee = ?1, status = ?2, claimed_by = NULL, lease_expires_at = NULL,
                 updated_at = ?3
             WHERE id = ?4",
            params![assignee, next_status, now, task_id],
        )
        .context("assign task")?;
        Self::log_event_locked(
            &db,
            task_id,
            actor_id,
            if assignee.is_some() { "assign" } else { "unassign" },
            Some(&current.status),
            Some(next_status),
            assignee.map(|a| format!("assignee={a}")).as_deref(),
            now,
        )?;
        Self::get_locked(&db, task_id)
    }

    /// Write a status change. Permission and transition rules live in the
    /// server; this only refuses unknown statuses and no-op races.
    pub fn set_status(
        &self,
        task_id: &str,
        expected_from: &str,
        to: &str,
        actor_id: &str,
        note: Option<&str>,
        branch: Option<&str>,
    ) -> Result<Task> {
        if !ALL_STATUSES.contains(&to) {
            return Err(anyhow!("unknown task status '{to}'"));
        }
        let now = unix_timestamp();
        let db = self.db.lock().expect("sqlite mutex poisoned");
        let changed = db
            .execute(
                "UPDATE tasks
                 SET status = ?1, branch = COALESCE(?2, branch), updated_at = ?3
                 WHERE id = ?4 AND status = ?5",
                params![to, branch, now, task_id, expected_from],
            )
            .context("update task status")?;
        if changed != 1 {
            return Err(anyhow!("task status changed concurrently; reload and retry"));
        }
        Self::log_event_locked(
            &db,
            task_id,
            actor_id,
            "status",
            Some(expected_from),
            Some(to),
            note,
            now,
        )?;
        Self::get_locked(&db, task_id)
    }

    pub fn list(&self, channel: &str) -> Result<Vec<Task>> {
        let db = self.db.lock().expect("sqlite mutex poisoned");
        let mut statement = db
            .prepare(&format!(
                "SELECT {TASK_COLUMNS}
                 FROM tasks
                 WHERE channel = ?1 AND status NOT IN ('done', 'merged', 'cancelled')
                 ORDER BY created_at ASC"
            ))
            .context("prepare task list")?;

        let tasks = statement
            .query_map(params![channel], Self::from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(tasks)
    }

    /// Open work owned by `principal_id`, across every channel.
    pub fn inbox(&self, principal_id: &str) -> Result<Vec<Task>> {
        let db = self.db.lock().expect("sqlite mutex poisoned");
        let mut statement = db
            .prepare(&format!(
                "SELECT {TASK_COLUMNS}
                 FROM tasks
                 WHERE status NOT IN ('done', 'merged', 'cancelled')
                   AND (assignee = ?1 OR (claimed_by = ?1 AND status = 'claimed'))
                 ORDER BY channel ASC, created_at ASC"
            ))
            .context("prepare inbox")?;
        let tasks = statement
            .query_map(params![principal_id], Self::from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(tasks)
    }

    /// Tasks waiting for review in `channel`.
    pub fn review_queue(&self, channel: &str) -> Result<Vec<Task>> {
        let db = self.db.lock().expect("sqlite mutex poisoned");
        let mut statement = db
            .prepare(&format!(
                "SELECT {TASK_COLUMNS}
                 FROM tasks
                 WHERE channel = ?1 AND status = 'review'
                 ORDER BY updated_at ASC"
            ))
            .context("prepare review queue")?;
        let tasks = statement
            .query_map(params![channel], Self::from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(tasks)
    }

    pub fn events(&self, task_id: &str) -> Result<Vec<TaskEvent>> {
        let db = self.db.lock().expect("sqlite mutex poisoned");
        let mut statement = db
            .prepare(
                "SELECT task_id, actor_id, action, from_status, to_status, note, created_at
                 FROM task_events WHERE task_id = ?1 ORDER BY id ASC",
            )
            .context("prepare task events")?;
        let events = statement
            .query_map(params![task_id], |row| {
                Ok(TaskEvent {
                    task_id: row.get(0)?,
                    actor_id: row.get(1)?,
                    action: row.get(2)?,
                    from_status: row.get(3)?,
                    to_status: row.get(4)?,
                    note: row.get(5)?,
                    created_at: row.get(6)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(events)
    }

    #[allow(clippy::too_many_arguments)]
    fn log_event_locked(
        db: &Connection,
        task_id: &str,
        actor_id: &str,
        action: &str,
        from_status: Option<&str>,
        to_status: Option<&str>,
        note: Option<&str>,
        now: i64,
    ) -> Result<()> {
        db.execute(
            "INSERT INTO task_events (task_id, actor_id, action, from_status, to_status, note, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![task_id, actor_id, action, from_status, to_status, note, now],
        )
        .context("record task event")?;
        Ok(())
    }

    fn get_locked(db: &Connection, task_id: &str) -> Result<Task> {
        db.query_row(
            &format!("SELECT {TASK_COLUMNS} FROM tasks WHERE id = ?1"),
            params![task_id],
            Self::from_row,
        )
        .context("load task")
    }

    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Task> {
        Ok(Task {
            id: row.get(0)?,
            channel: row.get(1)?,
            title: row.get(2)?,
            status: row.get(3)?,
            claimed_by: row.get(4)?,
            lease_expires_at: row.get(5)?,
            body: row.get(6)?,
            kind: row
                .get::<_, Option<String>>(7)?
                .unwrap_or_else(|| "task".to_string()),
            assignee: row.get(8)?,
            created_by: row.get(9)?,
            repo: row.get(10)?,
            branch: row.get(11)?,
            issue_url: row.get(12)?,
            created_at: row.get(13)?,
            updated_at: row.get(14)?,
        })
    }
}
