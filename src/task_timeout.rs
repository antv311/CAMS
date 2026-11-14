//! Durable execution heartbeats and atomic recovery. Chat/account presence does
//! not renew a task: a healthy worker must report progress on that task.
use super::*;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TaskTimeoutConfig {
    pub default_seconds: Option<u32>,
    pub priority_seconds: BTreeMap<String, u32>,
}

impl TaskTimeoutConfig {
    pub fn validate(&self) -> Result<()> {
        if self.default_seconds == Some(0) || self.priority_seconds.values().any(|v| *v == 0) {
            return Err(anyhow!("task timeouts must be positive seconds"));
        }
        for priority in self.priority_seconds.keys() {
            if !["urgent", "high", "normal", "low"].contains(&priority.as_str()) {
                return Err(anyhow!("unknown task timeout priority '{priority}'"));
            }
        }
        Ok(())
    }
}

pub struct TaskTimeout {
    pub task: Task,
    pub previous_holder: Option<String>,
    pub previous_status: String,
    pub note: String,
}

impl TaskStore {
    pub fn with_timeout_config(mut self, config: TaskTimeoutConfig) -> Result<Self> {
        config.validate()?;
        self.timeouts = config;
        Ok(self)
    }

    pub(super) fn timeout_seconds(&self, task: &Task) -> Option<i64> {
        task.timeout_seconds
            .or_else(|| self.timeouts.priority_seconds.get(&task.priority).copied())
            .or(self.timeouts.default_seconds)
            .map(i64::from)
            .or_else(|| (task.status == "claimed").then_some(self.lease_seconds))
    }

    pub(super) fn execution_expired(&self, task: &Task, now: i64) -> bool {
        if task.status == "claimed" {
            return task.lease_expires_at.is_none_or(|at| at <= now);
        }
        matches!(task.status.as_str(), "in_progress" | "testing")
            && self.timeout_seconds(task).is_some_and(|seconds| {
                task.execution_heartbeat_at
                    .unwrap_or(task.updated_at)
                    .saturating_add(seconds)
                    <= now
            })
    }

    pub fn set_timeout(&self, id: &str, seconds: Option<u32>, actor: &str) -> Result<Task> {
        if seconds == Some(0) {
            return Err(anyhow!("task timeout must be positive seconds"));
        }
        let now = unix_timestamp();
        let mut db = self.db.write()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = Self::get_locked(&tx, id)?;
        if TERMINAL_STATUSES.contains(&current.status.as_str()) {
            return Err(anyhow!("cannot configure a terminal task"));
        }
        // Changing policy grants one full interval, including an active claim.
        let mut configured = current.clone();
        configured.timeout_seconds = seconds;
        let deadline = (current.status == "claimed").then(|| {
            now.saturating_add(
                self.timeout_seconds(&configured)
                    .unwrap_or(self.lease_seconds),
            )
        });
        tx.execute("UPDATE tasks SET timeout_seconds = ?2, updated_at = ?3,
            execution_heartbeat_at = CASE WHEN status IN ('in_progress', 'testing') THEN ?3 ELSE NULL END,
            lease_expires_at = CASE WHEN status = 'claimed' THEN ?4 ELSE lease_expires_at END WHERE id = ?1",
            params![id, seconds, now, deadline])?;
        Self::log_event_locked(
            &tx,
            id,
            actor,
            "timeout_policy",
            Some(&current.status),
            Some(&current.status),
            Some(&format!(
                "timeout_seconds={}",
                seconds
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "inherit".into())
            )),
            now,
        )?;
        let task = Self::get_locked(&tx, id)?;
        tx.commit()?;
        Ok(task)
    }

    /// Renewal is authorized again under the same transaction as expiry checks.
    pub fn heartbeat(&self, id: &str, actor: &str) -> Result<Task> {
        let now = unix_timestamp();
        let mut db = self.db.write()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = Self::get_locked(&tx, id)?;
        let holder = if current.status == "claimed" {
            &current.claimed_by
        } else {
            &current.assignee
        };
        if holder.as_deref() != Some(actor)
            || !matches!(
                current.status.as_str(),
                "claimed" | "in_progress" | "testing"
            )
        {
            return Err(anyhow!("only the executing task holder can heartbeat"));
        }
        Self::require_unblocked_locked(&tx, id, true)?;
        if self.execution_expired(&current, now) {
            return Err(anyhow!(
                "task execution timed out; wait for recovery and start again"
            ));
        }
        let deadline = (current.status == "claimed").then(|| {
            now.saturating_add(self.timeout_seconds(&current).unwrap_or(self.lease_seconds))
        });
        tx.execute("UPDATE tasks SET execution_heartbeat_at = ?2, updated_at = ?2,
            lease_expires_at = CASE WHEN status = 'claimed' THEN ?3 ELSE lease_expires_at END WHERE id = ?1",
            params![id, now, deadline])?;
        let task = Self::get_locked(&tx, id)?;
        tx.commit()?;
        Ok(task)
    }

    pub fn recover_timeouts(&self, now: i64) -> Result<Vec<TaskTimeout>> {
        let mut db = self.db.write()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let tasks = {
            let mut stmt = tx.prepare(&format!(
                "SELECT {TASK_COLUMNS} FROM tasks
                WHERE status IN ('claimed', 'in_progress', 'testing') ORDER BY id"
            ))?;
            let rows = stmt.query_map([], Self::from_row)?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut recovered = Vec::new();
        for current in tasks {
            if !self.execution_expired(&current, now) {
                continue;
            }
            let next = if current.assignee.is_some() {
                "assigned"
            } else {
                "open"
            };
            let holder = current.claimed_by.clone().or(current.assignee.clone());
            let note = format!("execution timeout; previous_holder={} timeout_seconds={} lease_expires_at={:?} heartbeat_at={:?}; worker process/worktree preserved",
                holder.as_deref().unwrap_or("-"), self.timeout_seconds(&current).unwrap_or(0),
                current.lease_expires_at, current.execution_heartbeat_at);
            tx.execute("UPDATE tasks SET status = ?2, claimed_by = NULL, lease_expires_at = NULL,
                execution_heartbeat_at = NULL, review_round_json = NULL, updated_at = ?3 WHERE id = ?1",
                params![current.id, next, now])?;
            Self::log_event_locked(
                &tx,
                &current.id,
                "aircd",
                "timeout",
                Some(&current.status),
                Some(next),
                Some(&note),
                now,
            )?;
            recovered.push(TaskTimeout {
                task: Self::get_locked(&tx, &current.id)?,
                previous_holder: holder,
                previous_status: current.status,
                note,
            });
        }
        tx.commit()?;
        for event in &recovered {
            // Timeout events are committed before in-memory counters change.
            self.metrics
                .record_task_transition(&event.previous_status, &event.task.status);
        }
        Ok(recovered)
    }
}
