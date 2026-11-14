use anyhow::{anyhow, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use uuid::Uuid;

use crate::history::unix_timestamp;
use crate::store::SharedConnection;

const ALLOWED_STATUSES: &[&str] = &[
    "info",
    "ok",
    "completed",
    "needs_input",
    "failed",
    "warning",
];

#[derive(Clone, Debug, Serialize)]
pub struct Report {
    pub id: String,
    pub author_id: String,
    pub author_nick: String,
    pub channel: String,
    pub title: String,
    pub status: String,
    pub summary: String,
    pub body: String,
    pub task_id: Option<String>,
    pub git_repo: Option<String>,
    pub git_branch: Option<String>,
    pub git_commit: Option<String>,
    pub created_at: i64,
}

#[derive(Clone)]
pub struct ReportStore {
    db: SharedConnection,
}

pub struct NewReport<'a> {
    pub author_id: &'a str,
    pub channel: &'a str,
    pub title: &'a str,
    pub status: &'a str,
    pub summary: Option<&'a str>,
    pub body: &'a str,
    pub task_id: Option<&'a str>,
    pub git_repo: Option<&'a str>,
    pub git_branch: Option<&'a str>,
    pub git_commit: Option<&'a str>,
}

impl ReportStore {
    pub fn new(db: SharedConnection) -> Self {
        Self { db }
    }

    pub fn create(&self, input: NewReport<'_>) -> Result<Report> {
        let title = input.title.trim();
        if title.is_empty() || title.len() > 200 {
            return Err(anyhow!("Report title must be 1-200 characters"));
        }

        let status = input.status.trim().to_ascii_lowercase();
        if !ALLOWED_STATUSES.contains(&status.as_str()) {
            return Err(anyhow!(
                "Report status must be one of: {}",
                ALLOWED_STATUSES.join(", ")
            ));
        }

        let body = input.body.trim();
        if body.len() > 131_072 {
            return Err(anyhow!("Report body must be at most 128 KiB"));
        }

        let summary = match input.summary.map(str::trim).filter(|s| !s.is_empty()) {
            Some(explicit) => truncate_chars(explicit, 1000),
            None if !body.is_empty() => compact_preview(body, 200),
            None => title.to_string(),
        };

        let id = format!("rep_{}", Uuid::new_v4().simple());
        let created_at = unix_timestamp();
        let db = self.db.write()?;

        db.execute(
            "INSERT INTO reports
             (id, author_id, channel, title, status, summary, body, task_id, git_repo, git_branch, git_commit, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                id,
                input.author_id,
                input.channel,
                title,
                status,
                summary,
                body,
                input.task_id.map(str::trim).filter(|s| !s.is_empty()),
                input.git_repo.map(str::trim).filter(|s| !s.is_empty()),
                input.git_branch.map(str::trim).filter(|s| !s.is_empty()),
                input.git_commit.map(str::trim).filter(|s| !s.is_empty()),
                created_at
            ],
        )
        .context("insert report")?;

        Self::get_locked(&db, &id)?.ok_or_else(|| anyhow!("failed to reload report"))
    }

    pub fn get(&self, report_id: &str) -> Result<Option<Report>> {
        let db = self.db.read()?;
        Self::get_locked(&db, report_id)
    }

    pub fn list(
        &self,
        channel: Option<&str>,
        author_id: Option<&str>,
        limit: i64,
    ) -> Result<Vec<Report>> {
        let capped = limit.clamp(1, 200);
        let db = self.db.read()?;
        let mut statement = db
            .prepare(
                "SELECT r.id, r.author_id, COALESCE(p.nick, r.author_id), r.channel, r.title,
                        r.status, r.summary, r.body, r.task_id, r.git_repo, r.git_branch,
                        r.git_commit, r.created_at
                 FROM reports r
                 LEFT JOIN principals p ON p.id = r.author_id
                 WHERE (?1 IS NULL OR r.channel = ?1)
                   AND (?2 IS NULL OR r.author_id = ?2)
                 ORDER BY r.created_at DESC, r.id DESC
                 LIMIT ?3",
            )
            .context("prepare report list")?;

        let rows = statement
            .query_map(params![channel, author_id, capped], Self::from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn get_locked(db: &Connection, report_id: &str) -> Result<Option<Report>> {
        db.query_row(
            "SELECT r.id, r.author_id, COALESCE(p.nick, r.author_id), r.channel, r.title,
                    r.status, r.summary, r.body, r.task_id, r.git_repo, r.git_branch,
                    r.git_commit, r.created_at
             FROM reports r
             LEFT JOIN principals p ON p.id = r.author_id
             WHERE r.id = ?1",
            params![report_id],
            Self::from_row,
        )
        .optional()
        .context("load report")
    }

    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Report> {
        Ok(Report {
            id: row.get(0)?,
            author_id: row.get(1)?,
            author_nick: row.get(2)?,
            channel: row.get(3)?,
            title: row.get(4)?,
            status: row.get(5)?,
            summary: row.get(6)?,
            body: row.get(7)?,
            task_id: row.get(8)?,
            git_repo: row.get(9)?,
            git_branch: row.get(10)?,
            git_commit: row.get(11)?,
            created_at: row.get(12)?,
        })
    }
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let kept: String = text.chars().take(max_chars.saturating_sub(3)).collect();
    format!("{}...", kept.trim_end())
}

fn compact_preview(text: &str, max_chars: usize) -> String {
    let single_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&single_line, max_chars)
}
