//! Atomic task batches. Authorization is shared with the single-task workflow;
//! persistence and audit rows commit before any notifications are emitted.
use super::*;
use serde::Deserialize;

pub const MAX_TASK_BATCH: usize = 100;

pub struct BatchNewTask {
    pub key: Option<String>,
    pub task: NewTask,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskStatusChange {
    pub task_id: String,
    pub status: String,
    pub note: Option<String>,
    pub branch: Option<String>,
    pub report_body: Option<String>,
    pub report_id: Option<String>,
    pub commit: Option<String>,
}

fn check_batch_len(len: usize) -> Result<()> {
    if !(1..=MAX_TASK_BATCH).contains(&len) {
        return Err(anyhow!(
            "a task batch must contain 1..={MAX_TASK_BATCH} items"
        ));
    }
    Ok(())
}

fn resolve_reference(raw: &str, keys: &BTreeMap<String, String>) -> Result<String> {
    let raw = raw.trim();
    match raw.strip_prefix('$') {
        Some(key) => keys
            .get(key)
            .cloned()
            .ok_or_else(|| anyhow!("unknown batch key '${key}'")),
        None => Ok(raw.to_string()),
    }
}

/// Only parent fields used during authorization/inheritance are read from this
/// snapshot. The store creates the canonical rows in the final transaction.
fn parent_snapshot(id: &str, new: &NewTask) -> Task {
    Task {
        id: id.to_string(),
        channel: new.channel.clone(),
        title: new.title.clone(),
        status: if new.assignee.is_some() {
            "assigned"
        } else {
            "open"
        }
        .into(),
        claimed_by: None,
        lease_expires_at: None,
        execution_heartbeat_at: None,
        timeout_seconds: new.timeout_seconds,
        body: new.body.clone(),
        kind: new.kind.clone().unwrap_or_else(|| "task".into()),
        priority: new.priority.clone().unwrap_or_else(|| "normal".into()),
        assignee: new.assignee.clone(),
        created_by: new.created_by.clone(),
        repo: new.repo.clone(),
        branch: new.branch.clone(),
        issue_url: new.issue_url.clone(),
        created_at: 0,
        updated_at: 0,
        sprint: new.sprint.clone(),
        reviewer: new.reviewer.clone(),
        parent_task_id: new.parent_task_id.clone(),
        review_round: None,
        blocked_by: new.blocked_by.clone(),
    }
}

impl Server {
    pub fn create_task_batch_api(
        &self,
        actor: &Principal,
        entries: Vec<BatchNewTask>,
    ) -> Result<(Vec<Task>, BTreeMap<String, String>)> {
        let _guard = self.task_gate.lock().expect("task mutex poisoned");
        check_batch_len(entries.len())?;
        let ids: Vec<String> = entries
            .iter()
            .map(|_| format!("task_{}", uuid::Uuid::new_v4().simple()))
            .collect();
        let mut keys = BTreeMap::new();
        for (index, entry) in entries.iter().enumerate() {
            if let Some(key) = &entry.key {
                if key.is_empty()
                    || key.len() > 64
                    || !key
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                {
                    return Err(anyhow!(
                        "tasks[{index}]: key must be 1..64 ASCII letters, digits, '_' or '-'"
                    ));
                }
                if keys.insert(key.clone(), ids[index].clone()).is_some() {
                    return Err(anyhow!("tasks[{index}]: duplicate batch key '{key}'"));
                }
            }
        }
        let mut pending = Vec::new();
        for (index, entry) in entries.into_iter().enumerate() {
            let mut new = entry.task;
            new.parent_task_id = new
                .parent_task_id
                .as_deref()
                .map(|id| resolve_reference(id, &keys))
                .transpose()
                .with_context(|| format!("tasks[{index}]"))?
                .filter(|id| !id.is_empty());
            new.blocked_by = new
                .blocked_by
                .iter()
                .map(|id| resolve_reference(id, &keys))
                .collect::<Result<_>>()
                .with_context(|| format!("tasks[{index}]"))?;
            pending.push((index, new));
        }
        let local_ids: HashSet<&str> = ids.iter().map(String::as_str).collect();
        let mut parents = HashMap::new();
        let mut prepared = Vec::new();
        // Parent topology also permits forward references and inherited repo,
        // sprint and reviewer fields, without writing provisional DB rows.
        while !pending.is_empty() {
            let Some(position) = pending.iter().position(|(_, new)| {
                new.parent_task_id
                    .as_deref()
                    .is_none_or(|id| !local_ids.contains(id) || parents.contains_key(id))
            }) else {
                return Err(anyhow!("circular batch parent relationship"));
            };
            let (index, new) = pending.remove(position);
            let parent = new
                .parent_task_id
                .as_deref()
                .map(|id| {
                    parents
                        .get(id)
                        .cloned()
                        .map(Ok)
                        .unwrap_or_else(|| self.get_task_api(id))
                })
                .transpose()
                .with_context(|| format!("tasks[{index}]"))?;
            let new = self
                .prepare_task_creation(actor, new, parent.as_ref())
                .with_context(|| format!("tasks[{index}]"))?;
            parents.insert(ids[index].clone(), parent_snapshot(&ids[index], &new));
            prepared.push((index, new));
        }
        prepared.sort_by_key(|(index, _)| *index);
        let tasks = self.tasks.create_batch_full(
            prepared
                .into_iter()
                .map(|(index, new)| (ids[index].clone(), new))
                .collect(),
        )?;
        for task in &tasks {
            // Publication is after commit, so failed validation cannot send wakes.
            // A delivery failure must not turn a committed batch into a retryable failure.
            let result = self
                .join_channels(actor, std::slice::from_ref(&task.channel))
                .and_then(|_| self.announce_task_creation(actor, task));
            self.log_batch_delivery(result, &task.id);
        }
        Ok((tasks, keys))
    }

    pub fn bulk_task_status_api(
        &self,
        actor: &Principal,
        entries: Vec<TaskStatusChange>,
    ) -> Result<Vec<Task>> {
        let _guard = self.task_gate.lock().expect("task mutex poisoned");
        check_batch_len(entries.len())?;
        let mut seen = HashSet::new();
        let mut changes = Vec::with_capacity(entries.len());
        for (index, entry) in entries.into_iter().enumerate() {
            let id = entry.task_id.trim();
            if id.is_empty() || !seen.insert(id.to_string()) {
                return Err(anyhow!(
                    "updates[{index}]: task IDs must be nonblank and distinct"
                ));
            }
            let change = self
                .get_task_api(id)
                .and_then(|current| {
                    self.prepare_task_status_revision(
                        actor,
                        current,
                        &entry.status,
                        entry.note.as_deref(),
                        entry.branch.as_deref(),
                        entry.report_body.as_deref(),
                        entry.report_id.as_deref(),
                        entry.commit.as_deref(),
                    )
                })
                .with_context(|| format!("updates[{index}]"))?;
            changes.push(change);
        }
        let updates = self.tasks.set_status_batch(&changes, &actor.id)?;
        let tasks = updates.iter().map(|update| update.task.clone()).collect();
        for (change, update) in changes.iter().zip(updates) {
            let result = self.announce_task_status(actor, change, update).map(|_| ());
            self.log_batch_delivery(result, &change.current.id);
        }
        Ok(tasks)
    }

    fn log_batch_delivery(&self, result: Result<()>, task_id: &str) {
        if let Err(error) = result {
            self.logger.log(
                LogEvent::server_error(
                    "task.batch_delivery_error",
                    format!("committed task batch notification failed: {error:#}"),
                )
                .with_target(task_id.to_string()),
            );
        }
    }
}
