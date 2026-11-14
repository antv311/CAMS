use anyhow::{anyhow, Context, Result};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use uuid::Uuid;

use crate::history::unix_timestamp;
use crate::metrics::MetricsRegistry;
use crate::store::SharedConnection;

#[cfg(test)]
#[path = "task_cascade_tests.rs"]
mod cascade_tests;

#[path = "task_timeout.rs"]
mod timeout;
pub use timeout::TaskTimeoutConfig;

#[cfg(test)]
#[path = "task_timeout_tests.rs"]
mod timeout_tests;

#[path = "task_conditions.rs"]
mod conditions;
pub(crate) use conditions::deserialize_output;
pub use conditions::TaskCondition;

#[path = "task_attributes.rs"]
mod attributes;
pub use attributes::{normalize_tags, validate_metadata, TaskMetadata};

/// Statuses after which a task no longer appears in lists or inboxes.
pub const TERMINAL_STATUSES: &[&str] = &["done", "merged", "cancelled", "skipped"];

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
    "dormant",
    "skipped",
];

/// Default dispatch priority when omitted (`urgent`, `high`, `normal`, `low`).
pub const DEFAULT_PRIORITY: &str = "normal";

const PRIORITY_ORDER_SQL: &str = "CASE priority \
    WHEN 'urgent' THEN 0 \
    WHEN 'high' THEN 1 \
    WHEN 'normal' THEN 2 \
    WHEN 'low' THEN 3 \
    ELSE 2 END ASC";

const TASK_COLUMNS: &str = "id, channel, title, status, claimed_by, lease_expires_at, \
     body, kind, priority, assignee, created_by, repo, branch, issue_url, created_at, updated_at, sprint, \
     reviewer, parent_task_id, review_round_json, timeout_seconds, execution_heartbeat_at, \
     condition_json, optional, optional_enabled, output_json, tags_json, metadata_json";

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Task {
    pub id: String,
    pub channel: String,
    pub title: String,
    pub status: String,
    pub claimed_by: Option<String>,
    pub lease_expires_at: Option<i64>,
    pub timeout_seconds: Option<u32>,
    pub execution_heartbeat_at: Option<i64>,
    /// Full description: everything a new assignee needs without reading chat.
    pub body: Option<String>,
    /// `task`, `bug` or `feature`.
    pub kind: String,
    /// `urgent`, `high`, `normal` or `low`.
    pub priority: String,
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
    /// Task IDs that must be `done` or `merged` before this task can proceed.
    pub blocked_by: Vec<String>,
    pub condition: Option<TaskCondition>,
    pub optional: bool,
    pub optional_enabled: bool,
    pub output: Option<serde_json::Value>,
    pub tags: Vec<String>,
    pub metadata: TaskMetadata,
}

/// Summary of one dependency link (either a blocker or a dependent task).
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct TaskDependency {
    pub task_id: String,
    pub title: String,
    pub status: String,
    pub channel: String,
    pub repo: Option<String>,
    pub resolved: bool,
    pub cross_repo: bool,
    pub cross_project: bool,
}

/// A submission snapshot. A fresh revision is issued on every handoff, even
/// when the submitted commit or evidence happens to be identical.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
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
    pub priority: Option<String>,
    pub assignee: Option<String>,
    pub created_by: Option<String>,
    pub repo: Option<String>,
    pub branch: Option<String>,
    pub issue_url: Option<String>,
    pub sprint: Option<String>,
    pub reviewer: Option<String>,
    pub parent_task_id: Option<String>,
    pub blocked_by: Vec<String>,
    pub timeout_seconds: Option<u32>,
    pub condition: Option<TaskCondition>,
    pub optional: bool,
    pub tags: Vec<String>,
    pub metadata: TaskMetadata,
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

/// Committed descendant changes, including a lease holder whose lease was cleared.
pub struct TaskCascade {
    pub task: Task,
    pub action: &'static str,
    pub previous_status: String,
    pub previous_holder: Option<String>,
}

pub struct TaskUpdate {
    pub task: Task,
    pub cascaded: Vec<TaskCascade>,
}

/// A validated workflow mutation, prepared without side effects by the server.
pub(crate) struct PreparedTaskStatus {
    pub current: Task,
    pub to: String,
    pub note: Option<String>,
    pub branch: Option<String>,
    pub round: Option<ReviewRound>,
    pub output: Option<serde_json::Value>,
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
    /// `escalated` (waiting for the owner), `answered` or `cancelled`.
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
    timeouts: TaskTimeoutConfig,
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

pub fn normalize_priority(raw: Option<&str>) -> Result<String> {
    let clean = raw.unwrap_or(DEFAULT_PRIORITY).trim().to_ascii_lowercase();
    match clean.as_str() {
        "" | "normal" => Ok("normal".to_string()),
        "urgent" => Ok("urgent".to_string()),
        "high" => Ok("high".to_string()),
        "low" => Ok("low".to_string()),
        other => Err(anyhow!(
            "task priority must be urgent, high, normal or low (got '{other}')"
        )),
    }
}

/// Lower rank means higher urgency (`urgent=0` < `high=1` < `normal=2` < `low=3`).
pub fn priority_rank(priority: &str) -> u8 {
    match priority.trim().to_ascii_lowercase().as_str() {
        "urgent" => 0,
        "high" => 1,
        "normal" | "" => 2,
        "low" => 3,
        _ => 2,
    }
}

/// Whether a dependency status satisfies its `blocked_by` constraint.
pub fn is_dependency_resolved(status: &str) -> bool {
    matches!(status, "done" | "merged")
}

fn normalize_dep_ids(raw: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for item in raw {
        let trimmed = item.trim();
        if trimmed.is_empty() {
            continue;
        }
        let owned = trimmed.to_string();
        if seen.insert(owned.clone()) {
            out.push(owned);
        }
    }
    out
}

fn format_blocked_error(task_id: &str, blockers: &[TaskDependency]) -> String {
    let detail = blockers
        .iter()
        .map(|b| {
            format!(
                "{} [status={}, repo={}, channel={}]",
                b.task_id,
                b.status,
                b.repo.as_deref().unwrap_or("-"),
                b.channel
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("task {task_id} is blocked by unresolved dependencies: {detail}")
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
            timeouts: TaskTimeoutConfig::default(),
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
        let id = format!("task_{}", Uuid::new_v4().simple());
        Ok(self
            .create_batch_full(vec![(id, new)])
            .map_err(|error| anyhow!("{error:#}"))?
            .remove(0))
    }

    /// Insert every row and dependency edge under one writer transaction.
    /// New IDs are allocated by the server, never accepted from HTTP clients.
    pub(crate) fn create_batch_full(&self, entries: Vec<(String, NewTask)>) -> Result<Vec<Task>> {
        let now = unix_timestamp();
        let mut db = self.db.write()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for (index, (id, new)) in entries.iter().enumerate() {
            Self::insert_new_task_locked(&tx, id, new.clone(), now)
                .with_context(|| format!("tasks[{index}]"))?;
        }
        // All rows now exist, so forward dependencies can be validated.
        for (index, (id, new)) in entries.iter().enumerate() {
            Self::link_new_task_locked(&tx, id, new, now)
                .with_context(|| format!("tasks[{index}]"))?;
        }
        let settled = Self::settle_conditions_locked(&tx, "aircd", now)?;
        let tasks = entries
            .iter()
            .map(|(id, _)| Self::get_locked(&tx, id))
            .collect::<Result<Vec<_>>>()?;
        tx.commit()?;
        self.record_cascade_metrics(&settled);
        for task in &tasks {
            self.metrics.record_task_created(&task.kind);
        }
        Ok(tasks)
    }

    fn insert_new_task_locked(db: &Connection, id: &str, new: NewTask, now: i64) -> Result<()> {
        if new.timeout_seconds == Some(0) {
            return Err(anyhow!("task timeout must be positive seconds"));
        }
        let tags = normalize_tags(&new.tags)?;
        validate_metadata(&new.metadata)?;
        let kind = normalize_kind(new.kind.as_deref())?;
        let priority = normalize_priority(new.priority.as_deref())?;
        let assignee = clean_opt(new.assignee);
        let created_by = clean_opt(new.created_by);
        let status = if new.optional || new.condition.is_some() {
            "dormant"
        } else if assignee.is_some() {
            "assigned"
        } else {
            "open"
        };
        db.execute(
            "INSERT INTO tasks (id, channel, title, status, claimed_by, lease_expires_at,
                                body, kind, priority, assignee, created_by, repo, branch, issue_url,
                                created_at, updated_at, sprint, reviewer, parent_task_id, timeout_seconds,
                                condition_json, optional, tags_json, metadata_json)
             VALUES (?1, ?2, ?3, ?4, NULL, NULL, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21)",
            params![
                id,
                new.channel,
                new.title,
                status,
                clean_opt(new.body),
                kind,
                priority,
                assignee,
                created_by,
                clean_opt(new.repo),
                clean_opt(new.branch),
                clean_opt(new.issue_url),
                now,
                clean_opt(new.sprint),
                clean_opt(new.reviewer),
                clean_opt(new.parent_task_id),
                new.timeout_seconds,
                new.condition.as_ref().map(serde_json::to_string).transpose()?,
                new.optional,
                serde_json::to_string(&tags)?,
                serde_json::to_string(&new.metadata)?,
            ],
        )
        .context("insert task")?;

        Ok(())
    }

    fn link_new_task_locked(db: &Connection, id: &str, new: &NewTask, now: i64) -> Result<()> {
        let blocked_by = normalize_dep_ids(&new.blocked_by);
        let created_by = clean_opt(new.created_by.clone());
        Self::validate_condition_locked(db, &Self::get_locked(db, id)?)?;
        let status = if clean_opt(new.assignee.clone()).is_some() {
            "assigned"
        } else {
            "open"
        };
        Self::validate_dependencies_locked(db, id, &blocked_by, false)?;
        if let Some(parent_id) = clean_opt(new.parent_task_id.clone()) {
            let parent = Self::get_locked(db, &parent_id)?;
            if parent.channel != new.channel || TERMINAL_STATUSES.contains(&parent.status.as_str())
            {
                return Err(anyhow!(
                    "parent must be active and belong to the same project"
                ));
            }
            // Planning below dormant nodes is allowed; execution remains guarded.
            let blocked: bool = db.query_row(
                "WITH RECURSIVE ancestors(id) AS (
                SELECT ?1 UNION SELECT t.parent_task_id FROM tasks t JOIN ancestors a ON t.id = a.id
            ) SELECT EXISTS(SELECT 1 FROM tasks t JOIN ancestors a ON t.id = a.id
                WHERE t.status IN ('blocked', 'cancelled', 'skipped'))",
                [&parent_id],
                |row| row.get(0),
            )?;
            if blocked {
                return Err(anyhow!("parent task or ancestor is blocked"));
            }
            if Self::blocking_ancestor_locked(db, &parent_id, true)?
                .is_some_and(|(_, status)| status == "dormant")
            {
                db.execute("UPDATE tasks SET status = 'dormant' WHERE id = ?1", [id])?;
            }
        }
        for dep_id in &blocked_by {
            db.execute(
                "INSERT INTO task_dependencies (task_id, depends_on_task_id, created_by, created_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![id, dep_id, created_by, now],
            )
            .context("insert task dependency")?;
        }
        if !blocked_by.is_empty() {
            let actor = created_by.as_deref().unwrap_or("system");
            let note = format!("blocked_by={}", blocked_by.join(","));
            Self::log_event_locked(
                db,
                id,
                actor,
                "dependencies",
                Some(status),
                Some(status),
                Some(&note),
                now,
            )?;
        }

        Ok(())
    }

    pub fn get(&self, task_id: &str) -> Result<Option<Task>> {
        let db = self.db.read()?;
        Self::get_locked_opt(&db, task_id)
    }

    /// Include completed children so a lead can account for the whole decomposition.
    pub fn children(&self, parent_id: &str) -> Result<Vec<Task>> {
        let db = self.db.read()?;
        let mut statement = db.prepare(&format!(
            "SELECT {TASK_COLUMNS} FROM tasks WHERE parent_task_id = ?1 ORDER BY {PRIORITY_ORDER_SQL}, created_at ASC, rowid ASC"
        ))?;
        let mut tasks = statement
            .query_map([parent_id], Self::from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Self::attach_dependencies(&db, &mut tasks)?;
        Ok(tasks)
    }

    pub fn claim(&self, task_id: &str, principal_id: &str) -> Result<Task> {
        let now = unix_timestamp();
        let mut db = self.db.write()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = Self::get_locked_opt(&tx, task_id)?;
        if let Some(ref task) = current {
            let claimable = task.status == "open"
                || (task.status == "claimed" && task.lease_expires_at.is_none_or(|exp| exp <= now));
            if claimable {
                let blockers = Self::unresolved_blockers_locked(&tx, task)?;
                if !blockers.is_empty() {
                    return Err(anyhow!("{}", format_blocked_error(task_id, &blockers)));
                }
            }
        }
        let lease_expires_at = now.saturating_add(
            current
                .as_ref()
                .and_then(|t| self.timeout_seconds(t))
                .unwrap_or(self.lease_seconds),
        );
        let prev_status = current.as_ref().map(|t| t.status.clone());

        Self::require_unblocked_locked(&tx, task_id, true)?;

        // Only the lease-based flow is claimable; assigned work has an owner.
        let changed = tx
            .execute(
                "UPDATE tasks
                 SET status = 'claimed', claimed_by = ?1, lease_expires_at = ?2, updated_at = ?3
                 WHERE id = ?4
                   AND status IN ('open', 'claimed')
                   AND (
                     status = 'open'
                     OR lease_expires_at IS NULL
                     OR lease_expires_at <= ?3
                   )",
                params![principal_id, lease_expires_at, now, task_id],
            )
            .context("claim task")?;

        if changed != 1 {
            return Err(anyhow!("task is already claimed or done"));
        }

        if let Some(old) = current.as_ref().filter(|t| t.status == "claimed") {
            Self::log_event_locked(
                &tx,
                task_id,
                "aircd",
                "timeout",
                Some("claimed"),
                Some("claimed"),
                Some(&format!(
                    "expired lease reclaimed; previous_holder={}",
                    old.claimed_by.as_deref().unwrap_or("-")
                )),
                now,
            )?;
        }
        let task = Self::get_locked(&tx, task_id)?;
        tx.commit()?;
        self.metrics
            .record_task_transition(prev_status.as_deref().unwrap_or("open"), "claimed");
        Ok(task)
    }

    #[cfg(test)]
    pub fn done(&self, task_id: &str, principal_id: &str) -> Result<Task> {
        Ok(self.done_with_output(task_id, principal_id, None)?.task)
    }

    pub fn done_with_output(
        &self,
        task_id: &str,
        principal_id: &str,
        output: Option<&serde_json::Value>,
    ) -> Result<TaskUpdate> {
        let now = unix_timestamp();
        let mut db = self.db.write()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::require_unblocked_locked(&tx, task_id, true)?;
        if let Some(current) = Self::get_locked_opt(&tx, task_id)? {
            if self.execution_expired(&current, now) {
                return Err(anyhow!(
                    "task execution timed out; wait for recovery and start again"
                ));
            }
            if current.claimed_by.as_deref() == Some(principal_id) && current.status == "claimed" {
                let blockers = Self::unresolved_blockers_locked(&tx, &current)?;
                if !blockers.is_empty() {
                    return Err(anyhow!("{}", format_blocked_error(task_id, &blockers)));
                }
            }
        }

        let changed = tx
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

        let current = Self::get_locked(&tx, task_id)?;
        Self::save_output_locked(&tx, &current, "done", output)?;
        Self::log_event_locked(
            &tx,
            task_id,
            principal_id,
            "status",
            Some("claimed"),
            Some("done"),
            None,
            now,
        )?;
        let cascaded = Self::settle_conditions_locked(&tx, principal_id, now)?;
        let task = Self::get_locked(&tx, task_id)?;
        tx.commit()?;
        self.metrics.record_task_transition("claimed", "done");
        self.metrics.record_task_completed("done");
        self.record_cascade_metrics(&cascaded);
        Ok(TaskUpdate { task, cascaded })
    }

    pub fn release(&self, task_id: &str, principal_id: &str) -> Result<Task> {
        let now = unix_timestamp();
        let db = self.db.write()?;
        Self::require_unblocked_locked(&db, task_id, true)?;

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
        Self::require_unblocked_locked(&db, task_id, false)?;
        if current.status == "blocked" {
            return Err(anyhow!(
                "task is blocked; resolve its blocker before reassigning"
            ));
        }
        if TERMINAL_STATUSES.contains(&current.status.as_str()) {
            return Err(anyhow!(
                "task is {} and cannot be reassigned",
                current.status
            ));
        }
        let next_status = match (assignee, current.status.as_str()) {
            (_, "dormant") => "dormant",
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
            if assignee.is_some() {
                "assign"
            } else {
                "unassign"
            },
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

    /// Update the priority (`urgent`, `high`, `normal`, `low`) of an active task.
    pub fn set_priority(
        &self,
        task_id: &str,
        priority: &str,
        actor_id: &str,
    ) -> Result<(Task, bool)> {
        let normalized = normalize_priority(Some(priority))?;
        let now = unix_timestamp();
        let mut db = self.db.write()?;
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let current = Self::get_locked(&tx, task_id)?;
        if TERMINAL_STATUSES.contains(&current.status.as_str()) {
            return Err(anyhow!(
                "task is {} and its priority cannot be changed",
                current.status
            ));
        }
        if current.priority == normalized {
            return Ok((current, false));
        }
        tx.execute(
            "UPDATE tasks SET priority = ?1, updated_at = ?2 WHERE id = ?3",
            params![normalized, now, task_id],
        )
        .context("update task priority")?;
        let note = format!("priority={} (was {})", normalized, current.priority);
        Self::log_event_locked(
            &tx,
            task_id,
            actor_id,
            "priority",
            Some(&current.status),
            Some(&current.status),
            Some(&note),
            now,
        )?;
        let task = Self::get_locked(&tx, task_id)?;
        tx.commit()?;
        Ok((task, true))
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
        Ok(self
            .set_status_with_round(task_id, expected_from, to, actor_id, note, branch, None)?
            .task)
    }

    #[allow(clippy::too_many_arguments)]
    #[cfg(test)]
    pub fn set_status_with_round(
        &self,
        task_id: &str,
        expected_from: &str,
        to: &str,
        actor_id: &str,
        note: Option<&str>,
        branch: Option<&str>,
        round: Option<&ReviewRound>,
    ) -> Result<TaskUpdate> {
        let now = unix_timestamp();
        let mut db = self.db.write()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let update = self.set_status_locked(
            &tx,
            task_id,
            expected_from,
            to,
            actor_id,
            note,
            branch,
            round,
            now,
        )?;
        tx.commit()?;
        self.record_status_metrics(expected_from, &update);
        Ok(update)
    }

    pub(crate) fn set_status_batch(
        &self,
        changes: &[PreparedTaskStatus],
        actor_id: &str,
    ) -> Result<Vec<TaskUpdate>> {
        let now = unix_timestamp();
        let mut db = self.db.write()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut updates = Vec::with_capacity(changes.len());
        if changes.len() > 1 {
            let explicit: HashSet<&str> = changes.iter().map(|c| c.current.id.as_str()).collect();
            for (index, change) in changes
                .iter()
                .enumerate()
                .filter(|(_, c)| c.to == "cancelled")
            {
                let mut statement = tx.prepare(
                    "WITH RECURSIVE descendants(id) AS (
                        SELECT id FROM tasks WHERE parent_task_id = ?1
                        UNION SELECT t.id FROM tasks t JOIN descendants d ON t.parent_task_id = d.id
                     ) SELECT id FROM descendants",
                )?;
                let descendants =
                    statement.query_map([&change.current.id], |row| row.get::<_, String>(0))?;
                for id in descendants {
                    if explicit.contains(id?.as_str()) {
                        return Err(anyhow!("updates[{index}]: cancellation overlaps an explicit descendant in this batch; submit the ancestor alone"));
                    }
                }
            }
        }
        for (index, change) in changes.iter().enumerate() {
            let current = Self::get_locked(&tx, &change.current.id)?;
            // A cascade or concurrent mutation must never apply a stale authorization.
            if current != change.current {
                return Err(anyhow!("updates[{index}]: task changed concurrently or by an earlier cascade; reload and retry"));
            }
            let mut update = self
                .set_status_locked(
                    &tx,
                    &current.id,
                    &current.status,
                    &change.to,
                    actor_id,
                    change.note.as_deref(),
                    change.branch.as_deref(),
                    change.round.as_ref(),
                    now,
                )
                .with_context(|| format!("updates[{index}]"))?;
            Self::save_output_locked(&tx, &current, &change.to, change.output.as_ref())?;
            update
                .cascaded
                .extend(Self::settle_conditions_locked(&tx, actor_id, now)?);
            update.task = Self::get_locked(&tx, &current.id)?;
            if update
                .cascaded
                .iter()
                .any(|cascade| changes.iter().any(|c| c.current.id == cascade.task.id))
            {
                return Err(anyhow!("updates[{index}]: cascade overlaps an explicit task in this batch; submit the ancestor alone"));
            }
            updates.push(update);
        }
        tx.commit()?;
        for (change, update) in changes.iter().zip(&updates) {
            self.record_status_metrics(&change.current.status, update);
        }
        Ok(updates)
    }

    fn record_status_metrics(&self, from: &str, update: &TaskUpdate) {
        self.metrics
            .record_task_transition(from, &update.task.status);
        if TERMINAL_STATUSES.contains(&update.task.status.as_str()) {
            self.metrics.record_task_completed(&update.task.status);
        }
        self.record_cascade_metrics(&update.cascaded);
    }

    #[allow(clippy::too_many_arguments)]
    fn set_status_locked(
        &self,
        db: &Connection,
        task_id: &str,
        expected_from: &str,
        to: &str,
        actor_id: &str,
        note: Option<&str>,
        branch: Option<&str>,
        round: Option<&ReviewRound>,
        now: i64,
    ) -> Result<TaskUpdate> {
        if !ALL_STATUSES.contains(&to) {
            return Err(anyhow!("unknown task status '{to}'"));
        }
        let current = Self::get_locked(db, task_id)?;
        if current.status == "dormant" && to != "cancelled" {
            return Err(anyhow!("dormant task must activate before execution"));
        }
        if self.execution_expired(&current, now)
            && matches!(to, "in_progress" | "testing" | "review" | "done")
        {
            return Err(anyhow!(
                "task execution timed out; wait for recovery and start again"
            ));
        }
        if to != "cancelled" {
            Self::require_unblocked_locked(db, task_id, false)?;
        }
        if current.status != expected_from {
            return Err(anyhow!(
                "task status changed concurrently; reload and retry"
            ));
        }
        if matches!(
            to,
            "in_progress" | "testing" | "review" | "super_review" | "merged" | "done"
        ) {
            let blockers = Self::unresolved_blockers_locked(db, &current)?;
            if !blockers.is_empty() {
                return Err(anyhow!("{}", format_blocked_error(task_id, &blockers)));
            }
        }
        if round.is_some_and(|r| current.assignee.as_deref() != Some(r.assignee.as_str())) {
            return Err(anyhow!(
                "task assignee changed concurrently; reload and retry"
            ));
        }
        let saved_round = if to == "review" {
            round.map(serde_json::to_string).transpose()?
        } else if matches!(
            to,
            "changes_requested" | "in_progress" | "testing" | "cancelled" | "blocked"
        ) {
            None
        } else {
            current
                .review_round
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?
        };
        let changed = db
            .execute(
                "UPDATE tasks
                 SET status = ?1, branch = COALESCE(?2, branch), updated_at = ?3,
                     review_round_json = ?6,
                     claimed_by = CASE WHEN ?1 IN ('blocked', 'cancelled') THEN NULL ELSE claimed_by END,
                     lease_expires_at = CASE WHEN ?1 IN ('blocked', 'cancelled') THEN NULL ELSE lease_expires_at END
                 WHERE id = ?4 AND status = ?5",
                params![to, branch, now, task_id, expected_from, saved_round],
            )
            .context("update task status")?;
        if changed != 1 {
            return Err(anyhow!(
                "task status changed concurrently; reload and retry"
            ));
        }
        Self::log_event_locked(
            db,
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
                db,
                task_id,
                actor_id,
                "review_submission",
                Some(expected_from),
                Some(to),
                Some(&serde_json::to_string(round)?),
                now,
            )?;
        }
        if to == "cancelled" {
            Self::cancel_questions_locked(db, task_id)?;
            db.execute(
                "DELETE FROM task_parent_blocks WHERE task_id = ?1",
                [task_id],
            )?;
        }
        let cascaded = if matches!(to, "blocked" | "cancelled") || current.status == "blocked" {
            Self::cascade_locked(db, task_id, to, actor_id, now)?
        } else {
            Vec::new()
        };
        let task = Self::get_locked(db, task_id)?;
        Ok(TaskUpdate { task, cascaded })
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
        Self::require_unblocked_locked(&tx, task_id, true)?;
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
    pub fn ask(
        &self,
        task_id: &str,
        actor_id: &str,
        question: &str,
    ) -> Result<(TaskUpdate, TaskQuestion)> {
        let now = unix_timestamp();
        let mut db = self.db.write()?;
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let current = Self::get_locked(&tx, task_id)?;
        Self::require_unblocked_locked(&tx, task_id, true)?;
        if self.execution_expired(&current, now) {
            return Err(anyhow!(
                "task execution timed out; wait for recovery and start again"
            ));
        }
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
        tx.execute(
            "INSERT INTO task_questions
               (id, task_id, channel, asked_by, question, status, resume_status, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'open', ?6, ?7)",
            params![
                id,
                task_id,
                current.channel,
                actor_id,
                question,
                current.status,
                now
            ],
        )
        .context("record task question")?;
        tx.execute(
            "UPDATE tasks SET status = 'blocked', updated_at = ?1,
                claimed_by = NULL, lease_expires_at = NULL, review_round_json = NULL WHERE id = ?2",
            params![now, task_id],
        )
        .context("block task")?;
        Self::log_event_locked(
            &tx,
            task_id,
            actor_id,
            "ask",
            Some(&current.status),
            Some("blocked"),
            Some(question),
            now,
        )?;
        let cascaded = Self::cascade_locked(&tx, task_id, "blocked", actor_id, now)?;
        let task = Self::get_locked(&tx, task_id)?;
        let question = Self::question_locked(&tx, &id)?;
        tx.commit()?;
        self.metrics
            .record_task_transition(&current.status, "blocked");
        self.metrics.record_task_question("asked");
        self.record_cascade_metrics(&cascaded);
        Ok((TaskUpdate { task, cascaded }, question))
    }

    /// The unanswered question on a task, if any.
    pub fn pending_question(&self, task_id: &str) -> Result<Option<TaskQuestion>> {
        let db = self.db.read()?;
        Self::pending_question_locked(&db, task_id)
    }

    /// Answer the pending question and put the task back where it was.
    pub fn answer(
        &self,
        task_id: &str,
        actor_id: &str,
        answer: &str,
    ) -> Result<(TaskUpdate, TaskQuestion)> {
        let now = unix_timestamp();
        let mut db = self.db.write()?;
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        Self::require_unblocked_locked(&tx, task_id, false)?;
        let current = Self::get_locked(&tx, task_id)?;
        if current.status != "blocked" {
            return Err(anyhow!("task is not blocked"));
        }
        let question = Self::pending_question_locked(&tx, task_id)?
            .ok_or_else(|| anyhow!("task has no unanswered question"))?;
        tx.execute(
            "UPDATE task_questions
             SET status = 'answered', answer = ?1, answered_by = ?2, answered_at = ?3
             WHERE id = ?4",
            params![answer, actor_id, now, question.id],
        )
        .context("answer task question")?;
        tx.execute(
            "UPDATE tasks SET status = ?1, updated_at = ?2 WHERE id = ?3 AND status = 'blocked'",
            params![question.resume_status, now, task_id],
        )
        .context("unblock task")?;
        Self::log_event_locked(
            &tx,
            task_id,
            actor_id,
            "answer",
            Some("blocked"),
            Some(&question.resume_status),
            Some(answer),
            now,
        )?;
        let mut cascaded =
            Self::cascade_locked(&tx, task_id, &question.resume_status, actor_id, now)?;
        cascaded.extend(Self::settle_conditions_locked(&tx, actor_id, now)?);
        let task = Self::get_locked(&tx, task_id)?;
        let question = Self::question_locked(&tx, &question.id)?;
        tx.commit()?;
        self.metrics
            .record_task_transition("blocked", &question.resume_status);
        self.metrics.record_task_question("answered");
        self.record_cascade_metrics(&cascaded);
        Ok((TaskUpdate { task, cascaded }, question))
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

    fn record_cascade_metrics(&self, changes: &[TaskCascade]) {
        for change in changes {
            self.metrics
                .record_task_transition(&change.previous_status, &change.task.status);
            if TERMINAL_STATUSES.contains(&change.task.status.as_str()) {
                self.metrics.record_task_completed(&change.task.status);
            }
        }
    }

    fn inherited_pause_locked(db: &Connection, task_id: &str) -> Result<Option<String>> {
        Ok(db
            .query_row(
                "SELECT resume_status FROM task_parent_blocks WHERE task_id = ?1",
                [task_id],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// UNION (rather than UNION ALL) also bounds traversal of legacy cyclic links.
    fn blocking_ancestor_locked(
        db: &Connection,
        task_id: &str,
        include_self: bool,
    ) -> Result<Option<(String, String)>> {
        Ok(db
            .query_row(
                "WITH RECURSIVE ancestors(id) AS (
                SELECT CASE WHEN ?2 THEN id ELSE parent_task_id END FROM tasks WHERE id = ?1
                UNION SELECT t.parent_task_id FROM tasks t JOIN ancestors a ON t.id = a.id
             ) SELECT t.id, t.status FROM tasks t JOIN ancestors a ON t.id = a.id
               WHERE t.status IN ('blocked', 'cancelled', 'dormant', 'skipped') LIMIT 1",
                params![task_id, include_self],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?)
    }

    fn require_unblocked_locked(db: &Connection, task_id: &str, include_self: bool) -> Result<()> {
        if let Some((id, status)) = Self::blocking_ancestor_locked(db, task_id, include_self)? {
            return Err(anyhow!(
                "task {id} is {status}; descendant work cannot continue"
            ));
        }
        Ok(())
    }

    pub fn execution_blocked(&self, task_id: &str) -> Result<bool> {
        let db = self.db.read()?;
        Ok(Self::blocking_ancestor_locked(&db, task_id, true)?.is_some())
    }

    fn cancel_questions_locked(db: &Connection, task_id: &str) -> Result<()> {
        db.execute(
            "UPDATE task_questions SET status = 'cancelled'
             WHERE task_id = ?1 AND status IN ('open', 'escalated_pm', 'escalated')",
            [task_id],
        )?;
        Ok(())
    }

    /// Called inside the same write transaction as the parent transition.
    /// Traverse through completed children too, but never change their records.
    fn cascade_locked(
        db: &Connection,
        root: &str,
        to: &str,
        actor: &str,
        now: i64,
    ) -> Result<Vec<TaskCascade>> {
        let mut queue = VecDeque::from([root.to_string()]);
        let mut visited = HashSet::from([root.to_string()]);
        let mut changes = Vec::new();
        while let Some(parent) = queue.pop_front() {
            let children = {
                let mut statement = db.prepare(&format!(
                    "SELECT {TASK_COLUMNS} FROM tasks WHERE parent_task_id = ?1 ORDER BY created_at, rowid"
                ))?;
                let rows = statement.query_map([parent], Self::from_row)?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            for child in children {
                if !visited.insert(child.id.clone()) {
                    continue;
                }
                queue.push_back(child.id.clone());
                if TERMINAL_STATUSES.contains(&child.status.as_str()) {
                    continue;
                }
                let (next, action) = if matches!(to, "cancelled" | "skipped") {
                    Self::cancel_questions_locked(db, &child.id)?;
                    db.execute(
                        "DELETE FROM task_parent_blocks WHERE task_id = ?1",
                        [&child.id],
                    )?;
                    (
                        to.to_string(),
                        if to == "skipped" {
                            "parent_skip"
                        } else {
                            "parent_cancel"
                        },
                    )
                } else if to == "blocked" {
                    if child.status == "blocked" {
                        continue; // Its own question or an earlier ancestor pause remains authoritative.
                    }
                    let resume = match child.status.as_str() {
                        "claimed" => "open", // The lease is released, so it must be claimed again.
                        "review" | "super_review" => "testing", // Fresh evidence and votes are required.
                        other => other,
                    };
                    db.execute(
                        "INSERT OR IGNORE INTO task_parent_blocks(task_id, resume_status) VALUES (?1, ?2)",
                        params![child.id, resume],
                    )?;
                    ("blocked".to_string(), "parent_block")
                } else {
                    let Some(resume) = Self::inherited_pause_locked(db, &child.id)? else {
                        continue;
                    };
                    if child.status != "blocked"
                        || Self::blocking_ancestor_locked(db, &child.id, false)?.is_some()
                    {
                        continue;
                    }
                    db.execute(
                        "DELETE FROM task_parent_blocks WHERE task_id = ?1",
                        [&child.id],
                    )?;
                    (resume, "parent_resume")
                };
                db.execute(
                    "UPDATE tasks SET status = ?2, updated_at = ?3, claimed_by = NULL,
                        lease_expires_at = NULL, review_round_json = NULL WHERE id = ?1",
                    params![child.id, next, now],
                )?;
                let note = format!("ancestor_task={root} ancestor_status={to}");
                Self::log_event_locked(
                    db,
                    &child.id,
                    actor,
                    action,
                    Some(&child.status),
                    Some(&next),
                    Some(&note),
                    now,
                )?;
                changes.push(TaskCascade {
                    task: Self::get_locked(db, &child.id)?,
                    action,
                    previous_status: child.status,
                    previous_holder: child.claimed_by,
                });
            }
        }
        Ok(changes)
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
        self.list_filtered(channel, None)
    }

    pub fn list_filtered(&self, channel: &str, priority: Option<&str>) -> Result<Vec<Task>> {
        self.list_filtered_with_options(channel, priority, None, None)
    }

    pub fn list_filtered_with_options(
        &self,
        channel: &str,
        priority: Option<&str>,
        repo: Option<&str>,
        blocked: Option<bool>,
    ) -> Result<Vec<Task>> {
        self.list_filtered_with_tag(channel, priority, repo, blocked, None)
    }

    pub fn list_filtered_with_tag(
        &self,
        channel: &str,
        priority: Option<&str>,
        repo: Option<&str>,
        blocked: Option<bool>,
        tag: Option<&str>,
    ) -> Result<Vec<Task>> {
        if let Some(tag) = tag {
            normalize_tags(&[tag.to_string()])?;
        }
        let normalized_priority = priority
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(|p| normalize_priority(Some(p)))
            .transpose()?;
        let normalized_repo = repo.map(str::trim).filter(|r| !r.is_empty());
        let db = self.db.read()?;
        let mut statement = db
            .prepare(&format!(
                "SELECT {TASK_COLUMNS}
                 FROM tasks
                 WHERE channel = ?1
                   AND status NOT IN ('done', 'merged', 'cancelled', 'skipped')
                   AND (?2 IS NULL OR priority = ?2)
                   AND (?3 IS NULL OR LOWER(COALESCE(repo, '')) = LOWER(?3))
                   AND (?4 IS NULL OR EXISTS (SELECT 1 FROM json_each(tasks.tags_json) WHERE value = ?4))
                 ORDER BY {PRIORITY_ORDER_SQL}, created_at ASC, rowid ASC"
            ))
            .context("prepare task list")?;

        let mut tasks = statement
            .query_map(
                params![channel, normalized_priority, normalized_repo, tag],
                Self::from_row,
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Self::attach_dependencies(&db, &mut tasks)?;

        if let Some(want_blocked) = blocked {
            let ids: Vec<String> = tasks.iter().map(|t| t.id.clone()).collect();
            let unresolved = Self::unresolved_blockers_map_locked(&db, &ids)?;
            tasks.retain(|t| {
                let is_blocked = unresolved.get(&t.id).is_some_and(|b| !b.is_empty());
                is_blocked == want_blocked
            });
        }

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
                   AND (?3 = 1 OR status NOT IN ('done', 'merged', 'cancelled', 'skipped'))
                 ORDER BY updated_at DESC, created_at DESC, rowid DESC"
            ))
            .context("prepare full task list")?;

        let include_flag = if include_terminal { 1i64 } else { 0i64 };
        let mut tasks = statement
            .query_map(params![channel, sprint, include_flag], Self::from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Self::attach_dependencies(&db, &mut tasks)?;

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

    /// Open work owned by `principal_id`, across every channel, ordered by
    /// priority (`urgent` -> `high` -> `normal` -> `low`) then FIFO.
    pub fn inbox(&self, principal_id: &str) -> Result<Vec<Task>> {
        let db = self.db.read()?;
        let mut statement = db
            .prepare(&format!(
                "SELECT {TASK_COLUMNS}
                 FROM tasks
                 WHERE status NOT IN ('done', 'merged', 'cancelled', 'skipped', 'dormant')
                   AND (assignee = ?1 OR (claimed_by = ?1 AND status = 'claimed'))
                 ORDER BY {PRIORITY_ORDER_SQL}, created_at ASC, rowid ASC"
            ))
            .context("prepare inbox")?;
        let mut tasks = statement
            .query_map(params![principal_id], Self::from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Self::attach_dependencies(&db, &mut tasks)?;
        Ok(tasks)
    }

    /// Tasks waiting for review in `channel`, ordered by priority then FIFO.
    pub fn review_queue(&self, channel: &str) -> Result<Vec<Task>> {
        let db = self.db.read()?;
        let mut statement = db
            .prepare(&format!(
                "SELECT {TASK_COLUMNS}
                 FROM tasks
                 WHERE channel = ?1 AND status IN ('review', 'super_review')
                 ORDER BY {PRIORITY_ORDER_SQL}, updated_at ASC, rowid ASC"
            ))
            .context("prepare review queue")?;
        let mut tasks = statement
            .query_map(params![channel], Self::from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Self::attach_dependencies(&db, &mut tasks)?;
        Ok(tasks)
    }

    pub fn dependencies(&self, task_id: &str) -> Result<Vec<TaskDependency>> {
        let db = self.db.read()?;
        let task = Self::get_locked(&db, task_id)?;
        Self::dependencies_locked(&db, &task)
    }

    pub fn dependents(&self, task_id: &str) -> Result<Vec<TaskDependency>> {
        let db = self.db.read()?;
        let task = Self::get_locked(&db, task_id)?;
        Self::dependents_locked(&db, &task)
    }

    pub fn unresolved_blockers(&self, task_id: &str) -> Result<Vec<TaskDependency>> {
        let db = self.db.read()?;
        let task = Self::get_locked(&db, task_id)?;
        Self::unresolved_blockers_locked(&db, &task)
    }

    pub fn unresolved_blockers_map(
        &self,
        task_ids: &[String],
    ) -> Result<HashMap<String, Vec<TaskDependency>>> {
        let db = self.db.read()?;
        Self::unresolved_blockers_map_locked(&db, task_ids)
    }

    /// Replace all `blocked_by` dependencies for `task_id` atomically.
    pub fn set_dependencies(
        &self,
        task_id: &str,
        blocked_by: &[String],
        actor_id: &str,
    ) -> Result<(Task, bool)> {
        let normalized = normalize_dep_ids(blocked_by);
        let now = unix_timestamp();
        let mut db = self.db.write()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = Self::get_locked(&tx, task_id)?;
        if TERMINAL_STATUSES.contains(&current.status.as_str()) {
            return Err(anyhow!(
                "task is {} and its dependencies cannot be changed",
                current.status
            ));
        }
        Self::validate_dependencies_locked(&tx, task_id, &normalized, true)?;
        if current.blocked_by == normalized {
            return Ok((current, false));
        }
        tx.execute(
            "DELETE FROM task_dependencies WHERE task_id = ?1",
            params![task_id],
        )
        .context("clear task dependencies")?;
        for dep_id in &normalized {
            tx.execute(
                "INSERT INTO task_dependencies (task_id, depends_on_task_id, created_by, created_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![task_id, dep_id, actor_id, now],
            )
            .context("insert task dependency")?;
        }
        let note = if normalized.is_empty() {
            format!("blocked_by=- (was {})", current.blocked_by.join(","))
        } else {
            format!("blocked_by={}", normalized.join(","))
        };
        Self::log_event_locked(
            &tx,
            task_id,
            actor_id,
            "dependencies",
            Some(&current.status),
            Some(&current.status),
            Some(&note),
            now,
        )?;
        let task = Self::get_locked(&tx, task_id)?;
        tx.commit()?;
        Ok((task, true))
    }

    /// Add one or more `blocked_by` dependencies to `task_id` atomically.
    pub fn add_dependencies(
        &self,
        task_id: &str,
        blocked_by: &[String],
        actor_id: &str,
    ) -> Result<(Task, bool)> {
        let normalized = normalize_dep_ids(blocked_by);
        if normalized.is_empty() {
            return Err(anyhow!("at least one dependency task id is required"));
        }
        let now = unix_timestamp();
        let mut db = self.db.write()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = Self::get_locked(&tx, task_id)?;
        if TERMINAL_STATUSES.contains(&current.status.as_str()) {
            return Err(anyhow!(
                "task is {} and its dependencies cannot be changed",
                current.status
            ));
        }
        Self::validate_dependencies_locked(&tx, task_id, &normalized, false)?;
        let mut added = Vec::new();
        for dep_id in &normalized {
            if !current.blocked_by.contains(dep_id) {
                tx.execute(
                    "INSERT INTO task_dependencies (task_id, depends_on_task_id, created_by, created_at)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![task_id, dep_id, actor_id, now],
                )
                .context("insert task dependency")?;
                added.push(dep_id.clone());
            }
        }
        if added.is_empty() {
            return Ok((current, false));
        }
        let note = format!("added blocked_by={}", added.join(","));
        Self::log_event_locked(
            &tx,
            task_id,
            actor_id,
            "dependencies",
            Some(&current.status),
            Some(&current.status),
            Some(&note),
            now,
        )?;
        let task = Self::get_locked(&tx, task_id)?;
        tx.commit()?;
        Ok((task, true))
    }

    /// Remove a single `blocked_by` dependency from `task_id`.
    pub fn remove_dependency(
        &self,
        task_id: &str,
        depends_on_task_id: &str,
        actor_id: &str,
    ) -> Result<(Task, bool)> {
        let dep_id = depends_on_task_id.trim();
        let now = unix_timestamp();
        let mut db = self.db.write()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = Self::get_locked(&tx, task_id)?;
        if TERMINAL_STATUSES.contains(&current.status.as_str()) {
            return Err(anyhow!(
                "task is {} and its dependencies cannot be changed",
                current.status
            ));
        }
        let deleted = tx
            .execute(
                "DELETE FROM task_dependencies WHERE task_id = ?1 AND depends_on_task_id = ?2",
                params![task_id, dep_id],
            )
            .context("delete task dependency")?;
        if deleted == 0 {
            return Ok((current, false));
        }
        let note = format!("removed blocked_by={dep_id}");
        Self::log_event_locked(
            &tx,
            task_id,
            actor_id,
            "dependencies",
            Some(&current.status),
            Some(&current.status),
            Some(&note),
            now,
        )?;
        let task = Self::get_locked(&tx, task_id)?;
        tx.commit()?;
        Ok((task, true))
    }

    /// Every task in `channel` and/or `sprint`, closed ones included, oldest
    /// first. At least one filter must be given.
    pub fn history(&self, channel: Option<&str>, sprint: Option<&str>) -> Result<Vec<Task>> {
        if channel.is_none() && sprint.is_none() {
            return Err(anyhow!("task history needs a channel or a sprint"));
        }
        let db = self.db.read()?;
        let mut statement = db
            .prepare(&format!(
                "SELECT {TASK_COLUMNS}
                 FROM tasks
                 WHERE (?1 IS NULL OR channel = ?1) AND (?2 IS NULL OR sprint = ?2)
                 ORDER BY created_at ASC, rowid ASC"
            ))
            .context("prepare task history")?;
        let mut tasks = statement
            .query_map(params![channel, sprint], Self::from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Self::attach_dependencies(&db, &mut tasks)?;
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

    fn get_locked_opt(db: &Connection, task_id: &str) -> Result<Option<Task>> {
        let mut task = db
            .query_row(
                &format!("SELECT {TASK_COLUMNS} FROM tasks WHERE id = ?1"),
                params![task_id],
                Self::from_row,
            )
            .optional()
            .context("load task")?;
        if let Some(ref mut t) = task {
            t.blocked_by = Self::blocked_by_locked(db, &t.id)?;
        }
        Ok(task)
    }

    fn get_locked(db: &Connection, task_id: &str) -> Result<Task> {
        let mut task = db
            .query_row(
                &format!("SELECT {TASK_COLUMNS} FROM tasks WHERE id = ?1"),
                params![task_id],
                Self::from_row,
            )
            .context("load task")?;
        task.blocked_by = Self::blocked_by_locked(db, &task.id)?;
        Ok(task)
    }

    fn blocked_by_locked(db: &Connection, task_id: &str) -> Result<Vec<String>> {
        let mut stmt = db
            .prepare(
                "SELECT depends_on_task_id FROM task_dependencies
                 WHERE task_id = ?1
                 ORDER BY rowid ASC",
            )
            .context("prepare blocked_by")?;
        let deps = stmt
            .query_map(params![task_id], |row| row.get(0))?
            .collect::<std::result::Result<Vec<String>, _>>()?;
        Ok(deps)
    }

    fn attach_dependencies(db: &Connection, tasks: &mut [Task]) -> Result<()> {
        if tasks.is_empty() {
            return Ok(());
        }
        let wanted: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let mut stmt = db
            .prepare("SELECT task_id, depends_on_task_id FROM task_dependencies ORDER BY rowid ASC")
            .context("prepare all task_dependencies")?;
        let mut map: HashMap<String, Vec<String>> = HashMap::new();
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (tid, dep_id) = row?;
            if wanted.contains(tid.as_str()) {
                map.entry(tid).or_default().push(dep_id);
            }
        }
        for task in tasks {
            if let Some(deps) = map.remove(&task.id) {
                task.blocked_by = deps;
            }
        }
        Ok(())
    }

    fn validate_dependencies_locked(
        db: &Connection,
        task_id: &str,
        dep_ids: &[String],
        ignore_existing_edges_from_task: bool,
    ) -> Result<()> {
        for dep_id in dep_ids {
            if dep_id == task_id {
                return Err(anyhow!("a task cannot depend on itself"));
            }
            let dep_status: Option<String> = db
                .query_row(
                    "SELECT status FROM tasks WHERE id = ?1",
                    params![dep_id],
                    |row| row.get(0),
                )
                .optional()
                .context("check dependency task")?;
            let Some(status) = dep_status else {
                return Err(anyhow!("dependency task '{dep_id}' not found"));
            };
            if status == "cancelled" {
                return Err(anyhow!("cannot depend on cancelled task '{dep_id}'"));
            }
        }
        if let Some(cycle) =
            Self::would_create_cycle(db, task_id, dep_ids, ignore_existing_edges_from_task)?
        {
            return Err(anyhow!(
                "circular task dependency detected: {}",
                cycle.join(" -> ")
            ));
        }
        Ok(())
    }

    fn would_create_cycle(
        db: &Connection,
        task_id: &str,
        new_deps: &[String],
        ignore_existing_edges_from_task: bool,
    ) -> Result<Option<Vec<String>>> {
        let mut adj: HashMap<String, Vec<String>> = HashMap::new();
        let mut stmt = db
            .prepare("SELECT task_id, depends_on_task_id FROM task_dependencies ORDER BY rowid ASC")
            .context("load dependency graph")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (src, dst) = row?;
            if ignore_existing_edges_from_task && src == task_id {
                continue;
            }
            adj.entry(format!("finish:{src}"))
                .or_default()
                .push(format!("finish:{dst}"));
        }

        for dep in new_deps {
            adj.entry(format!("finish:{task_id}"))
                .or_default()
                .push(format!("finish:{dep}"));
        }
        for (source, target) in Self::condition_edges_locked(db)? {
            adj.entry(source).or_default().push(target);
        }
        for root in [format!("finish:{task_id}"), format!("activate:{task_id}")] {
            for start in adj.get(&root).into_iter().flatten() {
                let mut visited: HashSet<String> = HashSet::new();
                let mut parent: HashMap<String, String> = HashMap::new();
                let mut queue: VecDeque<String> = VecDeque::new();
                visited.insert(start.clone());
                queue.push_back(start.clone());
                while let Some(node) = queue.pop_front() {
                    if node == root {
                        let mut path = vec![root.clone()];
                        let mut cur = node;
                        while let Some(prev) = parent.get(&cur) {
                            path.push(prev.clone());
                            cur = prev.clone();
                        }
                        path.push(root.clone());
                        path.reverse();
                        return Ok(Some(path));
                    }
                    if let Some(neighbors) = adj.get(&node) {
                        for next in neighbors {
                            if visited.insert(next.clone()) {
                                parent.insert(next.clone(), node.clone());
                                queue.push_back(next.clone());
                            }
                        }
                    }
                }
            }
        }
        Ok(None)
    }

    fn dependencies_locked(db: &Connection, task: &Task) -> Result<Vec<TaskDependency>> {
        let mut stmt = db
            .prepare(
                "SELECT t.id, t.title, t.status, t.channel, t.repo
                 FROM task_dependencies d
                 JOIN tasks t ON t.id = d.depends_on_task_id
                 WHERE d.task_id = ?1
                 ORDER BY d.rowid ASC",
            )
            .context("prepare task dependencies")?;
        let deps = stmt
            .query_map(params![task.id], |row| {
                let status: String = row.get(2)?;
                let channel: String = row.get(3)?;
                let repo: Option<String> = row.get(4)?;
                Ok(TaskDependency {
                    task_id: row.get(0)?,
                    title: row.get(1)?,
                    resolved: is_dependency_resolved(&status),
                    status,
                    cross_project: channel != task.channel,
                    cross_repo: repo != task.repo,
                    channel,
                    repo,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(deps)
    }

    fn dependents_locked(db: &Connection, task: &Task) -> Result<Vec<TaskDependency>> {
        let resolved = is_dependency_resolved(&task.status);
        let mut stmt = db
            .prepare(
                "SELECT t.id, t.title, t.status, t.channel, t.repo
                 FROM task_dependencies d
                 JOIN tasks t ON t.id = d.task_id
                 WHERE d.depends_on_task_id = ?1
                 ORDER BY d.rowid ASC",
            )
            .context("prepare task dependents")?;
        let deps = stmt
            .query_map(params![task.id], |row| {
                let channel: String = row.get(3)?;
                let repo: Option<String> = row.get(4)?;
                Ok(TaskDependency {
                    task_id: row.get(0)?,
                    title: row.get(1)?,
                    status: row.get(2)?,
                    resolved,
                    cross_project: channel != task.channel,
                    cross_repo: repo != task.repo,
                    channel,
                    repo,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(deps)
    }

    fn unresolved_blockers_locked(db: &Connection, task: &Task) -> Result<Vec<TaskDependency>> {
        let deps = Self::dependencies_locked(db, task)?;
        Ok(deps.into_iter().filter(|d| !d.resolved).collect())
    }

    fn unresolved_blockers_map_locked(
        db: &Connection,
        task_ids: &[String],
    ) -> Result<HashMap<String, Vec<TaskDependency>>> {
        let mut out: HashMap<String, Vec<TaskDependency>> = HashMap::new();
        if task_ids.is_empty() {
            return Ok(out);
        }
        let wanted: HashSet<&str> = task_ids.iter().map(String::as_str).collect();
        let mut stmt = db
            .prepare(
                "SELECT d.task_id, src.channel, src.repo, dep.id, dep.title, dep.status, dep.channel, dep.repo
                 FROM task_dependencies d
                 JOIN tasks src ON src.id = d.task_id
                 JOIN tasks dep ON dep.id = d.depends_on_task_id
                 WHERE dep.status NOT IN ('done', 'merged')
                 ORDER BY d.rowid ASC",
            )
            .context("prepare unresolved blockers map")?;
        let rows = stmt.query_map([], |row| {
            let tid: String = row.get(0)?;
            let src_channel: String = row.get(1)?;
            let src_repo: Option<String> = row.get(2)?;
            let dep_id: String = row.get(3)?;
            let dep_title: String = row.get(4)?;
            let dep_status: String = row.get(5)?;
            let dep_channel: String = row.get(6)?;
            let dep_repo: Option<String> = row.get(7)?;
            Ok((
                tid,
                TaskDependency {
                    task_id: dep_id,
                    title: dep_title,
                    resolved: is_dependency_resolved(&dep_status),
                    status: dep_status,
                    cross_project: dep_channel != src_channel,
                    cross_repo: dep_repo != src_repo,
                    channel: dep_channel,
                    repo: dep_repo,
                },
            ))
        })?;
        for row in rows {
            let (tid, dep) = row?;
            if wanted.contains(tid.as_str()) {
                out.entry(tid).or_default().push(dep);
            }
        }
        Ok(out)
    }

    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Task> {
        Ok(Task {
            id: row.get(0)?,
            channel: row.get(1)?,
            title: row.get(2)?,
            status: row.get(3)?,
            claimed_by: row.get(4)?,
            lease_expires_at: row.get(5)?,
            timeout_seconds: row.get(20)?,
            execution_heartbeat_at: row.get(21)?,
            body: row.get(6)?,
            kind: row
                .get::<_, Option<String>>(7)?
                .unwrap_or_else(|| "task".to_string()),
            priority: row
                .get::<_, Option<String>>(8)?
                .unwrap_or_else(|| DEFAULT_PRIORITY.to_string()),
            assignee: row.get(9)?,
            created_by: row.get(10)?,
            repo: row.get(11)?,
            branch: row.get(12)?,
            issue_url: row.get(13)?,
            created_at: row.get(14)?,
            updated_at: row.get(15)?,
            sprint: row.get(16)?,
            reviewer: row.get(17)?,
            parent_task_id: row.get(18)?,
            review_round: row
                .get::<_, Option<String>>(19)?
                .map(|raw| {
                    serde_json::from_str(&raw).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            19,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })
                })
                .transpose()?,
            blocked_by: Vec::new(),
            condition: conditions::json_column(row, 22)?,
            optional: row.get(23)?,
            optional_enabled: row.get(24)?,
            output: conditions::json_column(row, 25)?,
            tags: conditions::json_column(row, 26)?.unwrap_or_default(),
            metadata: conditions::json_column(row, 27)?.unwrap_or_default(),
        })
    }
}
