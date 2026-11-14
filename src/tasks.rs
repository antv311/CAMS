use anyhow::{anyhow, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

use crate::history::unix_timestamp;
use crate::metrics::MetricsRegistry;
use crate::store::SharedConnection;

/// Statuses after which a task no longer appears in lists or inboxes.
pub const TERMINAL_STATUSES: &[&str] = &["done", "merged", "cancelled"];

/// Every status a task may hold. `open`/`claimed`/`done` are the original
/// lease-based flow; the rest model assigned work that goes through review.
pub const ALL_STATUSES: &[&str] = &[
    "open",
    "claimed",
    "assigned",
    "in_progress",
    "testing",
    "blocked",
    "review",
    "super_review",
    "changes_requested",
    "merged",
    "done",
    "cancelled",
];

const TASK_COLUMNS: &str = "id, channel, title, status, claimed_by, lease_expires_at, \
     body, kind, assignee, created_by, repo, branch, issue_url, created_at, updated_at, sprint, \
     reviewer, parent_task_id, review_round_json";

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
    /// The sprint room this task belongs to, such as `#quest-video-s2`.
    pub sprint: Option<String>,
    /// Account ID of the lead or PM who reviews this work.
    pub reviewer: Option<String>,
    pub parent_task_id: Option<String>,
    pub review_round: Option<ReviewRound>,
}

/// A submission snapshot. A fresh revision is issued on every handoff, even
/// when the submitted commit or evidence happens to be identical.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReviewRound {
    pub revision: String,
    pub group: String,
    pub assignee: String,
    pub commit: String,
    pub branch: String,
    pub evidence: String,
    pub peer_reviewers: Vec<String>,
    pub quorum: usize,
    pub votes: Vec<PeerVote>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PeerVote {
    pub reviewer: String,
    pub decision: String,
    pub note: String,
    pub created_at: i64,
}

impl ReviewRound {
    pub fn consensus(&self, eligible: &[String]) -> bool {
        !self.votes.iter().any(|vote| vote.decision == "reject")
            && self
                .votes
                .iter()
                .filter(|vote| vote.decision == "approve" && eligible.contains(&vote.reviewer))
                .count()
                >= self.quorum
    }
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
    pub sprint: Option<String>,
    pub reviewer: Option<String>,
    pub parent_task_id: Option<String>,
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

/// A question that stops a task until someone answers it.
#[derive(Clone, Debug, Serialize)]
pub struct TaskQuestion {
    pub id: String,
    pub task_id: String,
    pub channel: String,
    pub asked_by: String,
    pub question: String,
    /// `open` (waiting for the reviewer/PM), `escalated_pm` (waiting for the PM),
    /// `escalated` (waiting for the owner) or `answered`.
    pub status: String,
    /// The task status to return to once the question is answered.
    pub resume_status: String,
    pub answer: Option<String>,
    pub answered_by: Option<String>,
    pub escalation_note: Option<String>,
    pub created_at: i64,
    pub answered_at: Option<i64>,
}

const QUESTION_COLUMNS: &str = "id, task_id, channel, asked_by, question, status, \
     resume_status, answer, answered_by, escalation_note, created_at, answered_at";

#[derive(Clone)]
pub struct TaskStore {
    db: SharedConnection,
    lease_seconds: i64,
    metrics: Arc<MetricsRegistry>,
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
    pub fn new(db: SharedConnection, lease_seconds: i64) -> Self {
        Self::with_metrics(db, lease_seconds, Arc::new(MetricsRegistry::new()))
    }

    pub fn with_metrics(
        db: SharedConnection,
        lease_seconds: i64,
        metrics: Arc<MetricsRegistry>,
    ) -> Self {
        Self {
            db,
            lease_seconds,
            metrics,
        }
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

        let db = self.db.write()?;
        db.execute(
            "INSERT INTO tasks (id, channel, title, status, claimed_by, lease_expires_at,
                                body, kind, assignee, created_by, repo, branch, issue_url,
                                created_at, updated_at, sprint, reviewer, parent_task_id)
             VALUES (?1, ?2, ?3, ?4, NULL, NULL, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?12, ?13, ?14, ?15)",
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
                now,
                clean_opt(new.sprint),
                clean_opt(new.reviewer),
                clean_opt(new.parent_task_id)
            ],
        )
        .context("insert task")?;

        let task = Self::get_locked(&db, &id)?;
        self.metrics.record_task_created(&kind);
        Ok(task)
    }

    pub fn get(&self, task_id: &str) -> Result<Option<Task>> {
        let db = self.db.read()?;
        db.query_row(
            &format!("SELECT {TASK_COLUMNS} FROM tasks WHERE id = ?1"),
            params![task_id],
            Self::from_row,
        )
        .optional()
        .context("load task")
    }

    /// Include completed children so a lead can account for the whole decomposition.
    pub fn children(&self, parent_id: &str) -> Result<Vec<Task>> {
        let db = self.db.read()?;
        let mut statement = db.prepare(&format!(
            "SELECT {TASK_COLUMNS} FROM tasks WHERE parent_task_id = ?1 ORDER BY created_at, rowid"
        ))?;
        let tasks = statement
            .query_map([parent_id], Self::from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(tasks)
    }

    pub fn claim(&self, task_id: &str, principal_id: &str) -> Result<Task> {
        let now = unix_timestamp();
        let lease_expires_at = now + self.lease_seconds;
        let db = self.db.write()?;
        let prev_status: Option<String> = db
            .query_row(
                "SELECT status FROM tasks WHERE id = ?1",
                params![task_id],
                |row| row.get(0),
            )
            .optional()
            .context("load task before claim")?;

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

        let task = Self::get_locked(&db, task_id)?;
        self.metrics
            .record_task_transition(prev_status.as_deref().unwrap_or("open"), "claimed");
        Ok(task)
    }

    pub fn done(&self, task_id: &str, principal_id: &str) -> Result<Task> {
        let now = unix_timestamp();
        let db = self.db.write()?;

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

        let task = Self::get_locked(&db, task_id)?;
        self.metrics.record_task_transition("claimed", "done");
        self.metrics.record_task_completed("done");
        Ok(task)
    }

    pub fn release(&self, task_id: &str, principal_id: &str) -> Result<Task> {
        let now = unix_timestamp();
        let db = self.db.write()?;

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

        let task = Self::get_locked(&db, task_id)?;
        self.metrics.record_task_transition("claimed", "open");
        Ok(task)
    }

    /// Give the task to `assignee`, or back to the pool when `None`. Work that
    /// had not reached review restarts as `assigned`; a task in review or with
    /// changes requested keeps that status so the new owner sees where it is.
    pub fn assign(&self, task_id: &str, assignee: Option<&str>, actor_id: &str) -> Result<Task> {
        let now = unix_timestamp();
        let db = self.db.write()?;
        let current = Self::get_locked(&db, task_id)?;
        if TERMINAL_STATUSES.contains(&current.status.as_str()) {
            return Err(anyhow!("task is {} and cannot be reassigned", current.status));
        }
        let next_status = match (assignee, current.status.as_str()) {
            (None, _) => "open",
            // Reassignment invalidates both the evidence and peer votes.
            (Some(_), "review" | "super_review") if current.review_round.is_some() => "assigned",
            (Some(_), "review") => "review",
            (Some(_), "changes_requested") => "changes_requested",
            (Some(_), _) => "assigned",
        };
        db.execute(
            "UPDATE tasks
             SET assignee = ?1, status = ?2, claimed_by = NULL, lease_expires_at = NULL,
                 updated_at = ?3, review_round_json = NULL
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
        let task = Self::get_locked(&db, task_id)?;
        if current.status != next_status {
            self.metrics
                .record_task_transition(&current.status, next_status);
        }
        Ok(task)
    }

    /// Write a status change. Permission and transition rules live in the
    /// server; this only refuses unknown statuses and no-op races.
    #[cfg(test)]
    pub fn set_status(
        &self,
        task_id: &str,
        expected_from: &str,
        to: &str,
        actor_id: &str,
        note: Option<&str>,
        branch: Option<&str>,
    ) -> Result<Task> {
        self.set_status_with_round(task_id, expected_from, to, actor_id, note, branch, None)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn set_status_with_round(
        &self,
        task_id: &str,
        expected_from: &str,
        to: &str,
        actor_id: &str,
        note: Option<&str>,
        branch: Option<&str>,
        round: Option<&ReviewRound>,
    ) -> Result<Task> {
        if !ALL_STATUSES.contains(&to) {
            return Err(anyhow!("unknown task status '{to}'"));
        }
        let now = unix_timestamp();
        let mut db = self.db.write()?;
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let current = Self::get_locked(&tx, task_id)?;
        if round.is_some_and(|r| current.assignee.as_deref() != Some(r.assignee.as_str())) {
            return Err(anyhow!(
                "task assignee changed concurrently; reload and retry"
            ));
        }
        let saved_round = if to == "review" {
            round.map(serde_json::to_string).transpose()?
        } else if matches!(
            to,
            "changes_requested" | "in_progress" | "testing" | "cancelled"
        ) {
            None
        } else {
            current
                .review_round
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?
        };
        let changed = tx
            .execute(
                "UPDATE tasks
                 SET status = ?1, branch = COALESCE(?2, branch), updated_at = ?3,
                     review_round_json = ?6
                 WHERE id = ?4 AND status = ?5",
                params![to, branch, now, task_id, expected_from, saved_round],
            )
            .context("update task status")?;
        if changed != 1 {
            return Err(anyhow!("task status changed concurrently; reload and retry"));
        }
        Self::log_event_locked(
            &tx,
            task_id,
            actor_id,
            "status",
            Some(expected_from),
            Some(to),
            note,
            now,
        )?;
        if let Some(round) = round {
            Self::log_event_locked(
                &tx,
                task_id,
                actor_id,
                "review_submission",
                Some(expected_from),
                Some(to),
                Some(&serde_json::to_string(round)?),
                now,
            )?;
        }
        let task = Self::get_locked(&tx, task_id)?;
        tx.commit()?;
        self.metrics.record_task_transition(expected_from, to);
        if TERMINAL_STATUSES.contains(&to) {
            self.metrics.record_task_completed(to);
        }
        Ok(task)
    }

    /// Validate and replace a peer's vote atomically with phase advancement.
    pub fn peer_vote(
        &self,
        task_id: &str,
        revision: &str,
        actor_id: &str,
        decision: &str,
        note: &str,
        eligible: &[String],
    ) -> Result<Task> {
        if !matches!(decision, "approve" | "reject") || note.trim().is_empty() {
            return Err(anyhow!(
                "peer decision must be approve or reject with a nonblank review note"
            ));
        }
        let mut db = self.db.write()?;
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let current = Self::get_locked(&tx, task_id)?;
        if !matches!(current.status.as_str(), "review" | "super_review") {
            return Err(anyhow!("task is not awaiting review"));
        }
        let mut round = current
            .review_round
            .ok_or_else(|| anyhow!("task has no peer review round"))?;
        if round.revision != revision
            || current.assignee.as_deref() != Some(round.assignee.as_str())
        {
            return Err(anyhow!("stale review revision; reload the submitted task"));
        }
        if actor_id == round.assignee
            || !round.peer_reviewers.iter().any(|id| id == actor_id)
            || !eligible.iter().any(|id| id == actor_id)
        {
            return Err(anyhow!(
                "only a configured peer currently in the group and project may vote"
            ));
        }
        let now = unix_timestamp();
        round.votes.retain(|vote| vote.reviewer != actor_id);
        round.votes.push(PeerVote {
            reviewer: actor_id.to_string(),
            decision: decision.to_string(),
            note: note.trim().to_string(),
            created_at: now,
        });
        let to = if round.consensus(eligible) {
            "super_review"
        } else {
            "review"
        };
        tx.execute(
            "UPDATE tasks SET status = ?2, review_round_json = ?3, updated_at = ?4 WHERE id = ?1",
            params![task_id, to, serde_json::to_string(&round)?, now],
        )?;
        let evidence = serde_json::json!({"revision": revision, "commit": round.commit,
            "decision": decision, "note": note.trim()})
        .to_string();
        Self::log_event_locked(
            &tx,
            task_id,
            actor_id,
            "peer_review",
            Some(&current.status),
            Some(to),
            Some(&evidence),
            now,
        )?;
        let task = Self::get_locked(&tx, task_id)?;
        tx.commit()?;
        if current.status != to {
            self.metrics.record_task_transition(&current.status, to);
        }
        Ok(task)
    }

    /// The assignee is stuck: record the question and park the task.
    pub fn ask(&self, task_id: &str, actor_id: &str, question: &str) -> Result<(Task, TaskQuestion)> {
        let now = unix_timestamp();
        let db = self.db.write()?;
        let current = Self::get_locked(&db, task_id)?;
        if !matches!(
            current.status.as_str(),
            "assigned" | "in_progress" | "changes_requested"
        ) {
            return Err(anyhow!(
                "a question can only be asked on work in progress (task is {})",
                current.status
            ));
        }
        let id = format!("q_{}", Uuid::new_v4().simple());
        db.execute(
            "INSERT INTO task_questions
               (id, task_id, channel, asked_by, question, status, resume_status, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'open', ?6, ?7)",
            params![id, task_id, current.channel, actor_id, question, current.status, now],
        )
        .context("record task question")?;
        db.execute(
            "UPDATE tasks SET status = 'blocked', updated_at = ?1 WHERE id = ?2",
            params![now, task_id],
        )
        .context("block task")?;
        Self::log_event_locked(
            &db,
            task_id,
            actor_id,
            "ask",
            Some(&current.status),
            Some("blocked"),
            Some(question),
            now,
        )?;
        let out = (Self::get_locked(&db, task_id)?, Self::question_locked(&db, &id)?);
        self.metrics
            .record_task_transition(&current.status, "blocked");
        self.metrics.record_task_question("asked");
        Ok(out)
    }

    /// The unanswered question on a task, if any.
    pub fn pending_question(&self, task_id: &str) -> Result<Option<TaskQuestion>> {
        let db = self.db.read()?;
        Self::pending_question_locked(&db, task_id)
    }

    /// Answer the pending question and put the task back where it was.
    pub fn answer(&self, task_id: &str, actor_id: &str, answer: &str) -> Result<(Task, TaskQuestion)> {
        let now = unix_timestamp();
        let db = self.db.write()?;
        let question = Self::pending_question_locked(&db, task_id)?
            .ok_or_else(|| anyhow!("task has no unanswered question"))?;
        db.execute(
            "UPDATE task_questions
             SET status = 'answered', answer = ?1, answered_by = ?2, answered_at = ?3
             WHERE id = ?4",
            params![answer, actor_id, now, question.id],
        )
        .context("answer task question")?;
        db.execute(
            "UPDATE tasks SET status = ?1, updated_at = ?2 WHERE id = ?3 AND status = 'blocked'",
            params![question.resume_status, now, task_id],
        )
        .context("unblock task")?;
        Self::log_event_locked(
            &db,
            task_id,
            actor_id,
            "answer",
            Some("blocked"),
            Some(&question.resume_status),
            Some(answer),
            now,
        )?;
        let out = (
            Self::get_locked(&db, task_id)?,
            Self::question_locked(&db, &question.id)?,
        );
        self.metrics
            .record_task_transition("blocked", &question.resume_status);
        self.metrics.record_task_question("answered");
        Ok(out)
    }

    /// Hand an open question up to the owner.
    pub fn escalate(
        &self,
        task_id: &str,
        actor_id: &str,
        note: Option<&str>,
    ) -> Result<TaskQuestion> {
        self.escalate_question(task_id, actor_id, note, "escalated")
    }

    pub fn escalate_to_pm(
        &self,
        task_id: &str,
        actor_id: &str,
        note: Option<&str>,
    ) -> Result<TaskQuestion> {
        self.escalate_question(task_id, actor_id, note, "escalated_pm")
    }

    fn escalate_question(
        &self,
        task_id: &str,
        actor_id: &str,
        note: Option<&str>,
        to: &str,
    ) -> Result<TaskQuestion> {
        let now = unix_timestamp();
        let db = self.db.write()?;
        let question = Self::pending_question_locked(&db, task_id)?
            .filter(|q| q.status == "open" || (to == "escalated" && q.status == "escalated_pm"))
            .ok_or_else(|| anyhow!("task has no open question to escalate"))?;
        db.execute(
            "UPDATE task_questions SET status = ?1, escalation_note = ?2 WHERE id = ?3",
            params![to, note, question.id],
        )
        .context("escalate task question")?;
        Self::log_event_locked(
            &db,
            task_id,
            actor_id,
            if to == "escalated_pm" {
                "escalate_pm"
            } else {
                "escalate"
            },
            Some("blocked"),
            Some("blocked"),
            note,
            now,
        )?;
        let q = Self::question_locked(&db, &question.id)?;
        self.metrics.record_task_question(if to == "escalated_pm" {
            "escalated_pm"
        } else {
            "escalated"
        });
        Ok(q)
    }

    /// Questions with `status`, in one channel or everywhere.
    pub fn questions(&self, channel: Option<&str>, status: &str) -> Result<Vec<TaskQuestion>> {
        let db = self.db.read()?;
        let mut statement = db
            .prepare(&format!(
                "SELECT {QUESTION_COLUMNS} FROM task_questions
                 WHERE status = ?1 AND (?2 IS NULL OR channel = ?2)
                 ORDER BY created_at ASC"
            ))
            .context("prepare task questions")?;
        let rows = statement
            .query_map(params![status, channel], Self::question_from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn pending_question_locked(db: &Connection, task_id: &str) -> Result<Option<TaskQuestion>> {
        db.query_row(
            &format!(
                "SELECT {QUESTION_COLUMNS} FROM task_questions
                 WHERE task_id = ?1 AND status IN ('open', 'escalated_pm', 'escalated')
                 ORDER BY created_at DESC, rowid DESC LIMIT 1"
            ),
            params![task_id],
            Self::question_from_row,
        )
        .optional()
        .context("load pending question")
    }

    fn question_locked(db: &Connection, id: &str) -> Result<TaskQuestion> {
        db.query_row(
            &format!("SELECT {QUESTION_COLUMNS} FROM task_questions WHERE id = ?1"),
            params![id],
            Self::question_from_row,
        )
        .context("load task question")
    }

    fn question_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskQuestion> {
        Ok(TaskQuestion {
            id: row.get(0)?,
            task_id: row.get(1)?,
            channel: row.get(2)?,
            asked_by: row.get(3)?,
            question: row.get(4)?,
            status: row.get(5)?,
            resume_status: row.get(6)?,
            answer: row.get(7)?,
            answered_by: row.get(8)?,
            escalation_note: row.get(9)?,
            created_at: row.get(10)?,
            answered_at: row.get(11)?,
        })
    }

    pub fn list(&self, channel: &str) -> Result<Vec<Task>> {
        let db = self.db.read()?;
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

    /// List tasks across all or one project/sprint, optionally including terminal statuses.
    pub fn list_all(
        &self,
        channel: Option<&str>,
        sprint: Option<&str>,
        include_terminal: bool,
    ) -> Result<Vec<Task>> {
        let db = self.db.read()?;
        let mut statement = db
            .prepare(&format!(
                "SELECT {TASK_COLUMNS}
                 FROM tasks
                 WHERE (?1 IS NULL OR channel = ?1)
                   AND (?2 IS NULL OR sprint = ?2)
                   AND (?3 = 1 OR status NOT IN ('done', 'merged', 'cancelled'))
                 ORDER BY updated_at DESC, created_at DESC, rowid DESC"
            ))
            .context("prepare full task list")?;

        let include_flag = if include_terminal { 1i64 } else { 0i64 };
        let tasks = statement
            .query_map(params![channel, sprint, include_flag], Self::from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(tasks)
    }

    /// All currently unanswered questions (`open`, `escalated_pm`, `escalated`),
    /// optionally scoped to one project channel.
    pub fn pending_questions_all(&self, channel: Option<&str>) -> Result<Vec<TaskQuestion>> {
        let db = self.db.read()?;
        let mut statement = db
            .prepare(&format!(
                "SELECT {QUESTION_COLUMNS} FROM task_questions
                 WHERE status IN ('open', 'escalated_pm', 'escalated')
                   AND (?1 IS NULL OR channel = ?1)
                 ORDER BY created_at DESC, rowid DESC"
            ))
            .context("prepare pending questions")?;
        let rows = statement
            .query_map(params![channel], Self::question_from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Open work owned by `principal_id`, across every channel.
    pub fn inbox(&self, principal_id: &str) -> Result<Vec<Task>> {
        let db = self.db.read()?;
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
        let db = self.db.read()?;
        let mut statement = db
            .prepare(&format!(
                "SELECT {TASK_COLUMNS}
                 FROM tasks
                 WHERE channel = ?1 AND status IN ('review', 'super_review')
                 ORDER BY updated_at ASC"
            ))
            .context("prepare review queue")?;
        let tasks = statement
            .query_map(params![channel], Self::from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(tasks)
    }

    pub fn events(&self, task_id: &str) -> Result<Vec<TaskEvent>> {
        let db = self.db.read()?;
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
            sprint: row.get(15)?,
            reviewer: row.get(16)?,
            parent_task_id: row.get(17)?,
            review_round: row
                .get::<_, Option<String>>(18)?
                .map(|raw| {
                    serde_json::from_str(&raw).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            18,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })
                })
                .transpose()?,
        })
    }
}
