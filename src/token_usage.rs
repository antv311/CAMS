//! Model token accounting for tasks: every prompt/completion token a worker
//! (such as Bionic) spends on a task, rolled up through its subtasks, with an
//! optional ceiling (`token_budget`) per task.
//!
//! A task's budget covers the whole subtree under it: a subtask's tokens
//! count against its own budget and against every ancestor's budget, so a
//! lead who splits work cannot overspend the parent by fanning it out.
//! Recording never fails because a budget is spent (the tokens are already
//! gone); the report says the budget is exceeded and the worker stops.

use std::collections::BTreeMap;

use anyhow::{anyhow, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use crate::history::unix_timestamp;
use crate::store::SharedConnection;

/// One report larger than this is a client bug, not a real model call.
pub const MAX_TOKENS_PER_RECORD: i64 = 100_000_000;
/// Largest budget a task may carry.
pub const MAX_TOKEN_BUDGET: i64 = 1_000_000_000_000;
const MAX_LABEL_CHARS: usize = 200;
/// How deep the ancestor walk goes (the task tree is far shallower in practice).
const MAX_DEPTH: usize = 64;

/// Prompt, completion and total tokens.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct TokenCounts {
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
}

impl TokenCounts {
    pub fn new(prompt: i64, completion: i64, total: Option<i64>) -> Self {
        Self {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: total.unwrap_or(prompt + completion),
        }
    }

    fn add(&mut self, other: &TokenCounts) {
        self.prompt_tokens += other.prompt_tokens;
        self.completion_tokens += other.completion_tokens;
        self.total_tokens += other.total_tokens;
    }

    pub fn is_zero(&self) -> bool {
        self.prompt_tokens == 0 && self.completion_tokens == 0 && self.total_tokens == 0
    }
}

/// Tokens one worker session spent on one task with one model.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SessionUsage {
    pub session_id: Option<String>,
    pub recorded_by: String,
    pub model: Option<String>,
    pub records: i64,
    #[serde(flatten)]
    pub tokens: TokenCounts,
    pub first_at: i64,
    pub last_at: i64,
}

/// Tokens of one descendant task (its own spend, not its subtree).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SubtaskUsage {
    pub task_id: String,
    pub parent_task_id: Option<String>,
    #[serde(flatten)]
    pub tokens: TokenCounts,
}

/// A budget on the task or one of its ancestors and how much of it is left.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct BudgetState {
    pub task_id: String,
    pub token_budget: i64,
    /// Total tokens spent in that task's whole subtree.
    pub spent: i64,
    /// `token_budget - spent`; zero or negative once exhausted.
    pub remaining: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TokenReport {
    pub task_id: String,
    /// Tokens recorded directly on this task.
    pub own: TokenCounts,
    /// Tokens recorded on all descendant tasks.
    pub subtasks: TokenCounts,
    /// `own + subtasks`.
    pub total: TokenCounts,
    /// This task's own ceiling, if any.
    pub token_budget: Option<i64>,
    /// Tightest remaining allowance over this task's and its ancestors' budgets.
    pub remaining: Option<i64>,
    /// The task whose budget gives `remaining`.
    pub limited_by: Option<String>,
    /// True once any applicable budget is spent.
    pub exceeded: bool,
    /// Every applicable budget, this task first, then ancestors upward.
    pub budgets: Vec<BudgetState>,
    pub by_session: Vec<SessionUsage>,
    pub by_subtask: Vec<SubtaskUsage>,
}

/// A usage report from a worker.
pub struct NewUsage<'a> {
    pub task_id: &'a str,
    pub recorded_by: &'a str,
    pub session_id: Option<&'a str>,
    pub model: Option<&'a str>,
    pub tokens: TokenCounts,
}

pub fn migrate(connection: &Connection) -> Result<()> {
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS task_token_usage (
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               task_id TEXT NOT NULL,
               recorded_by TEXT NOT NULL,
               session_id TEXT,
               model TEXT,
               prompt_tokens INTEGER NOT NULL,
               completion_tokens INTEGER NOT NULL,
               total_tokens INTEGER NOT NULL,
               created_at INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_task_token_usage_task
               ON task_token_usage (task_id, created_at);
             CREATE TABLE IF NOT EXISTS task_token_budgets (
               task_id TEXT PRIMARY KEY NOT NULL,
               token_budget INTEGER NOT NULL,
               set_by TEXT NOT NULL,
               updated_at INTEGER NOT NULL
             );",
        )
        .context("migrate task token usage")
}

fn clean_label(raw: Option<&str>) -> Option<String> {
    raw.map(str::trim).filter(|s| !s.is_empty()).map(|s| {
        s.chars()
            .filter(|c| !c.is_control())
            .take(MAX_LABEL_CHARS)
            .collect()
    })
}

/// Reject negative or absurd counts, and a total below prompt + completion.
pub fn validate_counts(tokens: &TokenCounts) -> Result<()> {
    for (name, value) in [
        ("prompt_tokens", tokens.prompt_tokens),
        ("completion_tokens", tokens.completion_tokens),
        ("total_tokens", tokens.total_tokens),
    ] {
        if value < 0 {
            return Err(anyhow!("{name} cannot be negative"));
        }
        if value > MAX_TOKENS_PER_RECORD {
            return Err(anyhow!(
                "{name} is {value}; one report may hold at most {MAX_TOKENS_PER_RECORD}"
            ));
        }
    }
    if tokens.total_tokens < tokens.prompt_tokens + tokens.completion_tokens {
        return Err(anyhow!(
            "total_tokens ({}) is less than prompt_tokens + completion_tokens ({})",
            tokens.total_tokens,
            tokens.prompt_tokens + tokens.completion_tokens
        ));
    }
    if tokens.is_zero() {
        return Err(anyhow!("a usage report needs at least one token"));
    }
    Ok(())
}

#[derive(Clone)]
pub struct TokenUsageStore {
    db: SharedConnection,
}

impl TokenUsageStore {
    pub fn new(db: SharedConnection) -> Self {
        Self { db }
    }

    /// Record one usage report and return the task's report after it. Logs a
    /// `token_budget_exceeded` task event the first time a report pushes a
    /// budget (on the task or an ancestor) to zero or below.
    pub fn record(&self, new: NewUsage<'_>) -> Result<TokenReport> {
        validate_counts(&new.tokens)?;
        let session_id = clean_label(new.session_id);
        let model = clean_label(new.model);
        let now = unix_timestamp();
        let mut db = self.db.write()?;
        let tx = db.transaction().context("begin token usage insert")?;
        let before = budget_states_locked(&tx, new.task_id)?;
        tx.execute(
            "INSERT INTO task_token_usage
               (task_id, recorded_by, session_id, model, prompt_tokens, completion_tokens,
                total_tokens, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                new.task_id,
                new.recorded_by,
                session_id,
                model,
                new.tokens.prompt_tokens,
                new.tokens.completion_tokens,
                new.tokens.total_tokens,
                now,
            ],
        )
        .context("save token usage")?;
        let report = report_locked(&tx, new.task_id)?;
        for state in &report.budgets {
            let was_open = before
                .iter()
                .find(|b| b.task_id == state.task_id)
                .is_none_or(|b| b.remaining > 0);
            if was_open && state.remaining <= 0 {
                tx.execute(
                    "INSERT INTO task_events
                       (task_id, actor_id, action, from_status, to_status, note, created_at)
                     VALUES (?1, ?2, 'token_budget_exceeded', NULL, NULL, ?3, ?4)",
                    params![
                        state.task_id,
                        new.recorded_by,
                        format!(
                            "token budget {} spent: {} tokens used (last report on {})",
                            state.token_budget, state.spent, new.task_id
                        ),
                        now,
                    ],
                )
                .context("record budget event")?;
            }
        }
        tx.commit().context("commit token usage")?;
        Ok(report)
    }

    /// Set (`Some`) or clear (`None`) a task's token budget.
    pub fn set_budget(&self, task_id: &str, budget: Option<i64>, actor_id: &str) -> Result<()> {
        if let Some(budget) = budget {
            if budget <= 0 {
                return Err(anyhow!(
                    "token_budget must be positive (or null to clear it)"
                ));
            }
            if budget > MAX_TOKEN_BUDGET {
                return Err(anyhow!("token_budget may be at most {MAX_TOKEN_BUDGET}"));
            }
        }
        let now = unix_timestamp();
        let mut db = self.db.write()?;
        let tx = db.transaction().context("begin token budget update")?;
        let note = match budget {
            Some(budget) => {
                tx.execute(
                    "INSERT INTO task_token_budgets (task_id, token_budget, set_by, updated_at)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(task_id) DO UPDATE SET
                       token_budget = excluded.token_budget,
                       set_by = excluded.set_by,
                       updated_at = excluded.updated_at",
                    params![task_id, budget, actor_id, now],
                )
                .context("save token budget")?;
                format!("token budget set to {budget}")
            }
            None => {
                tx.execute(
                    "DELETE FROM task_token_budgets WHERE task_id = ?1",
                    params![task_id],
                )
                .context("clear token budget")?;
                "token budget cleared".to_string()
            }
        };
        tx.execute(
            "INSERT INTO task_events (task_id, actor_id, action, from_status, to_status, note, created_at)
             VALUES (?1, ?2, 'token_budget', NULL, NULL, ?3, ?4)",
            params![task_id, actor_id, note, now],
        )
        .context("record token budget event")?;
        tx.commit().context("commit token budget")?;
        Ok(())
    }

    #[cfg(test)]
    pub fn budget(&self, task_id: &str) -> Result<Option<i64>> {
        let db = self.db.read()?;
        budget_locked(&db, task_id)
    }

    pub fn report(&self, task_id: &str) -> Result<TokenReport> {
        let db = self.db.read()?;
        report_locked(&db, task_id)
    }

    /// Own and subtree totals plus the task's budget for many tasks at once
    /// (for exports). Tasks with no usage and no budget are left out.
    pub fn summaries(&self, task_ids: &[&str]) -> Result<BTreeMap<String, TokenReport>> {
        let db = self.db.read()?;
        let mut out = BTreeMap::new();
        for id in task_ids {
            let report = report_locked(&db, id)?;
            if !report.total.is_zero() || !report.budgets.is_empty() {
                out.insert((*id).to_string(), report);
            }
        }
        Ok(out)
    }
}

fn budget_locked(db: &Connection, task_id: &str) -> Result<Option<i64>> {
    db.query_row(
        "SELECT token_budget FROM task_token_budgets WHERE task_id = ?1",
        params![task_id],
        |row| row.get(0),
    )
    .optional()
    .context("load token budget")
}

/// The task's ancestors, nearest first (cycle-safe).
fn ancestors_locked(db: &Connection, task_id: &str) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    let mut current = task_id.to_string();
    for _ in 0..MAX_DEPTH {
        let parent: Option<String> = db
            .query_row(
                "SELECT parent_task_id FROM tasks WHERE id = ?1",
                params![current],
                |row| row.get(0),
            )
            .optional()
            .context("load parent task")?
            .flatten();
        match parent {
            Some(parent) if parent != task_id && !out.contains(&parent) => {
                out.push(parent.clone());
                current = parent;
            }
            _ => break,
        }
    }
    Ok(out)
}

/// Tokens spent by each task in the subtree rooted at `task_id` (root included).
fn subtree_usage_locked(db: &Connection, task_id: &str) -> Result<Vec<SubtaskUsage>> {
    let mut statement = db
        .prepare(
            "WITH RECURSIVE tree(id, parent, depth) AS (
               SELECT ?1, NULL, 0
               UNION
               SELECT t.id, t.parent_task_id, tree.depth + 1
                 FROM tasks t JOIN tree ON t.parent_task_id = tree.id
                WHERE tree.depth < ?2
             )
             SELECT tree.id, tree.parent,
                    COALESCE(SUM(u.prompt_tokens), 0),
                    COALESCE(SUM(u.completion_tokens), 0),
                    COALESCE(SUM(u.total_tokens), 0)
               FROM tree LEFT JOIN task_token_usage u ON u.task_id = tree.id
              GROUP BY tree.id, tree.parent
              ORDER BY MIN(tree.depth), tree.id",
        )
        .context("prepare subtree token usage")?;
    let rows = statement
        .query_map(params![task_id, MAX_DEPTH as i64], |row| {
            Ok(SubtaskUsage {
                task_id: row.get(0)?,
                parent_task_id: row.get(1)?,
                tokens: TokenCounts {
                    prompt_tokens: row.get(2)?,
                    completion_tokens: row.get(3)?,
                    total_tokens: row.get(4)?,
                },
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn subtree_total_locked(db: &Connection, task_id: &str) -> Result<i64> {
    Ok(subtree_usage_locked(db, task_id)?
        .iter()
        .map(|row| row.tokens.total_tokens)
        .sum())
}

/// Budgets on the task and its ancestors, nearest first.
fn budget_states_locked(db: &Connection, task_id: &str) -> Result<Vec<BudgetState>> {
    let mut chain = vec![task_id.to_string()];
    chain.extend(ancestors_locked(db, task_id)?);
    let mut out = Vec::new();
    for id in chain {
        if let Some(budget) = budget_locked(db, &id)? {
            let spent = subtree_total_locked(db, &id)?;
            out.push(BudgetState {
                task_id: id,
                token_budget: budget,
                spent,
                remaining: budget - spent,
            });
        }
    }
    Ok(out)
}

fn sessions_locked(db: &Connection, task_id: &str) -> Result<Vec<SessionUsage>> {
    let mut statement = db
        .prepare(
            "SELECT session_id, recorded_by, model, COUNT(*),
                    SUM(prompt_tokens), SUM(completion_tokens), SUM(total_tokens),
                    MIN(created_at), MAX(created_at)
               FROM task_token_usage WHERE task_id = ?1
              GROUP BY session_id, recorded_by, model
              ORDER BY MIN(created_at), MIN(id)",
        )
        .context("prepare session token usage")?;
    let rows = statement
        .query_map(params![task_id], |row| {
            Ok(SessionUsage {
                session_id: row.get(0)?,
                recorded_by: row.get(1)?,
                model: row.get(2)?,
                records: row.get(3)?,
                tokens: TokenCounts {
                    prompt_tokens: row.get(4)?,
                    completion_tokens: row.get(5)?,
                    total_tokens: row.get(6)?,
                },
                first_at: row.get(7)?,
                last_at: row.get(8)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn report_locked(db: &Connection, task_id: &str) -> Result<TokenReport> {
    let tree = subtree_usage_locked(db, task_id)?;
    let mut own = TokenCounts::default();
    let mut subtasks = TokenCounts::default();
    let mut by_subtask = Vec::new();
    for row in tree {
        if row.task_id == task_id {
            own = row.tokens;
        } else {
            subtasks.add(&row.tokens);
            by_subtask.push(row);
        }
    }
    let mut total = own;
    total.add(&subtasks);
    let budgets = budget_states_locked(db, task_id)?;
    let tightest = budgets.iter().min_by_key(|b| b.remaining);
    Ok(TokenReport {
        task_id: task_id.to_string(),
        own,
        subtasks,
        total,
        token_budget: budgets
            .first()
            .filter(|b| b.task_id == task_id)
            .map(|b| b.token_budget),
        remaining: tightest.map(|b| b.remaining),
        limited_by: tightest.map(|b| b.task_id.clone()),
        exceeded: budgets.iter().any(|b| b.remaining <= 0),
        budgets,
        by_session: sessions_locked(db, task_id)?,
        by_subtask,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store;
    use crate::tasks::{NewTask, TaskStore};

    fn usage<'a>(task_id: &'a str, session: &'a str, p: i64, c: i64) -> NewUsage<'a> {
        NewUsage {
            task_id,
            recorded_by: "worker",
            session_id: Some(session),
            model: Some("gemma"),
            tokens: TokenCounts::new(p, c, None),
        }
    }

    fn tree(db: &SharedConnection) -> Result<(String, String, String)> {
        let tasks = TaskStore::new(db.clone(), 60);
        let parent = tasks.create("#proj", "parent")?;
        let child = tasks.create_full(NewTask {
            channel: "#proj".into(),
            title: "child".into(),
            parent_task_id: Some(parent.id.clone()),
            ..NewTask::default()
        })?;
        let grandchild = tasks.create_full(NewTask {
            channel: "#proj".into(),
            title: "grandchild".into(),
            parent_task_id: Some(child.id.clone()),
            ..NewTask::default()
        })?;
        Ok((parent.id, child.id, grandchild.id))
    }

    #[test]
    fn records_and_rolls_up_through_subtasks() -> Result<()> {
        let db = store::open(":memory:")?;
        let (parent, child, grandchild) = tree(&db)?;
        let store = TokenUsageStore::new(db.clone());
        store.record(usage(&parent, "s1", 100, 20))?;
        store.record(usage(&parent, "s1", 50, 5))?;
        store.record(usage(&child, "s2", 10, 1))?;
        store.record(usage(&grandchild, "s3", 7, 3))?;

        let report = store.report(&parent)?;
        assert_eq!(report.own, TokenCounts::new(150, 25, None));
        assert_eq!(report.subtasks, TokenCounts::new(17, 4, None));
        assert_eq!(report.total.total_tokens, 196);
        assert_eq!(report.by_session.len(), 1);
        assert_eq!(report.by_session[0].records, 2);
        assert_eq!(report.by_session[0].session_id.as_deref(), Some("s1"));
        assert_eq!(report.by_subtask.len(), 2);
        assert_eq!(report.by_subtask[0].task_id, child);
        assert_eq!(report.by_subtask[1].task_id, grandchild);
        assert_eq!(report.remaining, None);
        assert!(!report.exceeded);

        let child_report = store.report(&child)?;
        assert_eq!(child_report.own.total_tokens, 11);
        assert_eq!(child_report.total.total_tokens, 21);
        Ok(())
    }

    #[test]
    fn ancestor_budget_limits_subtasks_and_logs_one_event() -> Result<()> {
        let db = store::open(":memory:")?;
        let (parent, child, grandchild) = tree(&db)?;
        let store = TokenUsageStore::new(db.clone());
        store.set_budget(&parent, Some(1_000), "pm")?;
        store.set_budget(&child, Some(5_000), "lead")?;
        store.record(usage(&parent, "s1", 600, 0))?;

        let report = store.report(&grandchild)?;
        assert_eq!(report.token_budget, None);
        assert_eq!(report.remaining, Some(400));
        assert_eq!(report.limited_by.as_deref(), Some(parent.as_str()));
        assert_eq!(report.budgets.len(), 2);
        assert_eq!(report.budgets[0].task_id, child);

        let after = store.record(usage(&grandchild, "s3", 300, 150))?;
        assert!(after.exceeded);
        assert_eq!(after.remaining, Some(-50));
        store.record(usage(&grandchild, "s3", 1, 0))?;
        let events: i64 = db.read()?.query_row(
            "SELECT COUNT(*) FROM task_events WHERE task_id = ?1 AND action = 'token_budget_exceeded'",
            params![parent],
            |row| row.get(0),
        )?;
        assert_eq!(events, 1, "the crossing is logged once");

        // Raising the budget reopens it.
        store.set_budget(&parent, Some(10_000), "pm")?;
        let reopened = store.report(&grandchild)?;
        assert!(!reopened.exceeded);
        assert_eq!(reopened.limited_by.as_deref(), Some(child.as_str()));
        store.set_budget(&child, None, "lead")?;
        assert_eq!(store.budget(&child)?, None);
        Ok(())
    }

    #[test]
    fn rejects_bad_counts_and_budgets() -> Result<()> {
        let store = TokenUsageStore::new(store::open(":memory:")?);
        assert!(store.record(usage("t", "s", -1, 0)).is_err());
        assert!(store.record(usage("t", "s", 0, 0)).is_err());
        assert!(store
            .record(usage("t", "s", MAX_TOKENS_PER_RECORD + 1, 0))
            .is_err());
        let mut low_total = usage("t", "s", 5, 5);
        low_total.tokens.total_tokens = 9;
        assert!(store.record(low_total).is_err());
        let mut high_total = usage("t", "s", 5, 5);
        high_total.tokens.total_tokens = 12; // e.g. reasoning tokens counted only in the total
        assert_eq!(store.record(high_total)?.own.total_tokens, 12);
        assert!(store.set_budget("t", Some(0), "pm").is_err());
        assert!(store
            .set_budget("t", Some(MAX_TOKEN_BUDGET + 1), "pm")
            .is_err());
        Ok(())
    }

    #[test]
    fn summaries_skip_tasks_without_usage_or_budget() -> Result<()> {
        let db = store::open(":memory:")?;
        let (parent, child, grandchild) = tree(&db)?;
        let store = TokenUsageStore::new(db);
        store.record(usage(&child, "s", 3, 4))?;
        let map = store.summaries(&[&parent, &child, &grandchild])?;
        assert_eq!(map.len(), 2);
        assert_eq!(map[&parent].subtasks.total_tokens, 7);
        assert!(!map.contains_key(&grandchild));
        Ok(())
    }
}
