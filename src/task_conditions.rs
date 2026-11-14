//! Persisted, one-shot branch decisions. No report parsing or executable expressions.
use super::*;
use serde_json::Value;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskCondition {
    ParentOutputEquals { pointer: String, value: Value },
    ParentOutputExists { pointer: String },
    SiblingSucceeded { task_id: String },
}

impl TaskCondition {
    pub(crate) fn source<'a>(&'a self, parent: &'a str) -> &'a str {
        match self {
            Self::SiblingSucceeded { task_id } => task_id,
            _ => parent,
        }
    }

    pub(crate) fn resolve_sibling(
        &mut self,
        resolve: impl FnOnce(&str) -> Result<String>,
    ) -> Result<()> {
        if let Self::SiblingSucceeded { task_id } = self {
            *task_id = resolve(task_id)?;
        }
        Ok(())
    }
}

impl TaskStore {
    pub(super) fn validate_condition_locked(db: &Connection, task: &Task) -> Result<()> {
        if task.condition.is_none() && !task.optional {
            return Ok(());
        }
        let parent_id = task
            .parent_task_id
            .as_deref()
            .ok_or_else(|| anyhow!("optional and conditional tasks must be child tasks"))?;
        if let Some(condition) = &task.condition {
            if serde_json::to_vec(condition)?.len() > 65536 {
                return Err(anyhow!("condition exceeds 65536 bytes"));
            }
            match condition {
                TaskCondition::ParentOutputEquals { pointer, .. }
                | TaskCondition::ParentOutputExists { pointer } => {
                    let bytes = pointer.as_bytes();
                    if pointer.len() > 1024
                        || (!pointer.is_empty() && !pointer.starts_with('/'))
                        || bytes.iter().enumerate().any(|(i, b)| {
                            *b == b'~' && !matches!(bytes.get(i + 1), Some(b'0' | b'1'))
                        })
                    {
                        return Err(anyhow!(
                            "condition pointer must be a valid JSON Pointer (at most 1024 bytes)"
                        ));
                    }
                }
                TaskCondition::SiblingSucceeded { task_id } => {
                    let sibling = Self::get_locked(db, task_id)?;
                    if sibling.id == task.id
                        || sibling.parent_task_id.as_deref() != Some(parent_id)
                        || sibling.channel != task.channel
                    {
                        return Err(anyhow!(
                            "sibling condition must name a different child of the same parent"
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    pub(super) fn condition_edges_locked(db: &Connection) -> Result<Vec<(String, String)>> {
        let mut stmt = db.prepare(&format!("SELECT {TASK_COLUMNS} FROM tasks"))?;
        let tasks = stmt
            .query_map([], Self::from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut edges = Vec::new();
        for task in &tasks {
            if TERMINAL_STATUSES.contains(&task.status.as_str()) {
                continue;
            }
            // Activation and completion are separate nodes: a parent may wait
            // for a child's completion without preventing that child's activation.
            edges.push((
                format!("finish:{}", task.id),
                format!("activate:{}", task.id),
            ));
            let Some(parent) = task.parent_task_id.as_deref() else {
                continue;
            };
            edges.push((
                format!("activate:{}", task.id),
                format!("activate:{parent}"),
            ));
            if task.status == "dormant"
                || (task.status == "blocked"
                    && Self::inherited_pause_locked(db, &task.id)?.as_deref() == Some("dormant"))
            {
                if let Some(condition) = &task.condition {
                    edges.push((
                        format!("activate:{}", task.id),
                        format!("finish:{}", condition.source(parent)),
                    ));
                }
            }
        }
        Ok(edges)
    }

    fn condition_result_locked(db: &Connection, task: &Task) -> Result<Option<bool>> {
        let Some(condition) = &task.condition else {
            return Ok(Some(true));
        };
        let parent = task
            .parent_task_id
            .as_deref()
            .ok_or_else(|| anyhow!("condition has no parent"))?;
        let source = Self::get_locked(db, condition.source(parent))?;
        if !TERMINAL_STATUSES.contains(&source.status.as_str()) {
            return Ok(None);
        }
        if !is_dependency_resolved(&source.status) {
            return Ok(Some(false));
        }
        let empty = serde_json::json!({});
        let output = source.output.as_ref().unwrap_or(&empty);
        Ok(Some(match condition {
            TaskCondition::ParentOutputEquals { pointer, value } => {
                output.pointer(pointer) == Some(value)
            }
            TaskCondition::ParentOutputExists { pointer } => output.pointer(pointer).is_some(),
            TaskCondition::SiblingSucceeded { .. } => true,
        }))
    }

    pub(crate) fn validate_output(output: Option<&Value>, to: &str) -> Result<()> {
        if let Some(output) = output {
            if !is_dependency_resolved(to) || !output.is_object() {
                return Err(anyhow!("output must be a JSON object supplied with successful completion (done/merged)"));
            }
            if serde_json::to_vec(output)?.len() > 65536 {
                return Err(anyhow!("task output exceeds 65536 bytes"));
            }
        }
        Ok(())
    }

    pub(super) fn save_output_locked(
        db: &Connection,
        task: &Task,
        to: &str,
        output: Option<&Value>,
    ) -> Result<()> {
        Self::validate_output(output, to)?;
        if is_dependency_resolved(to) {
            if task.output.is_some() {
                return Err(anyhow!("completion output is immutable"));
            }
            let empty = serde_json::json!({});
            db.execute(
                "UPDATE tasks SET output_json = ?2 WHERE id = ?1",
                params![task.id, serde_json::to_string(output.unwrap_or(&empty))?],
            )?;
        }
        Ok(())
    }

    /// Reach a fixed point so a skipped sibling also resolves downstream branches.
    /// Paused ancestors defer evaluation; a resumed branch retains its dormant state.
    pub(super) fn settle_conditions_locked(
        db: &Connection,
        actor: &str,
        now: i64,
    ) -> Result<Vec<TaskCascade>> {
        let mut changes = Vec::new();
        loop {
            let tasks = {
                let mut stmt = db.prepare(&format!("SELECT {TASK_COLUMNS} FROM tasks WHERE status = 'dormant' ORDER BY created_at, rowid"))?;
                let rows = stmt.query_map([], Self::from_row)?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            let before = changes.len();
            for task in tasks {
                if Self::get_locked(db, &task.id)?.status != "dormant" {
                    continue;
                }
                if Self::blocking_ancestor_locked(db, &task.id, false)?.is_some() {
                    continue;
                }
                let Some(result) = Self::condition_result_locked(db, &task)? else {
                    continue;
                };
                if result && task.optional && !task.optional_enabled {
                    continue;
                }
                let next = if !result {
                    "skipped"
                } else if task.assignee.is_some() {
                    "assigned"
                } else {
                    "open"
                };
                let action = if result {
                    "condition_activate"
                } else {
                    "condition_skip"
                };
                db.execute("UPDATE tasks SET status = ?2, updated_at = ?3 WHERE id = ?1 AND status = 'dormant'",
                    params![task.id, next, now])?;
                Self::log_event_locked(
                    db,
                    &task.id,
                    actor,
                    action,
                    Some("dormant"),
                    Some(next),
                    Some(if result {
                        "activation gates satisfied"
                    } else {
                        "condition resolved false"
                    }),
                    now,
                )?;
                changes.push(TaskCascade {
                    task: Self::get_locked(db, &task.id)?,
                    action,
                    previous_status: "dormant".into(),
                    previous_holder: None,
                });
                if !result {
                    changes.extend(Self::cascade_locked(db, &task.id, "skipped", actor, now)?);
                }
            }
            if changes.len() == before {
                break;
            }
        }
        Ok(changes)
    }

    pub fn enable_optional(&self, task_id: &str, actor: &str) -> Result<TaskUpdate> {
        let now = unix_timestamp();
        let mut db = self.db.write()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = Self::get_locked(&tx, task_id)?;
        Self::require_unblocked_locked(&tx, task_id, false)?;
        if current.status != "dormant" || !current.optional || current.optional_enabled {
            return Err(anyhow!(
                "only a dormant, disabled optional task can be enabled"
            ));
        }
        tx.execute(
            "UPDATE tasks SET optional_enabled = 1, updated_at = ?2 WHERE id = ?1",
            params![task_id, now],
        )?;
        Self::log_event_locked(
            &tx,
            task_id,
            actor,
            "optional_enable",
            Some("dormant"),
            Some("dormant"),
            None,
            now,
        )?;
        let cascaded = Self::settle_conditions_locked(&tx, actor, now)?;
        let task = Self::get_locked(&tx, task_id)?;
        tx.commit()?;
        self.record_cascade_metrics(&cascaded);
        Ok(TaskUpdate { task, cascaded })
    }
}

pub(crate) fn deserialize_output<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

pub(super) fn json_column<T: serde::de::DeserializeOwned>(
    row: &rusqlite::Row<'_>,
    index: usize,
) -> rusqlite::Result<Option<T>> {
    row.get::<_, Option<String>>(index)?
        .map(|raw| {
            serde_json::from_str(&raw).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    index,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task_counters(store: &TaskStore) -> Vec<String> {
        store
            .metrics
            .render_prometheus(&Default::default())
            .lines()
            .filter(|line| {
                line.starts_with("aircd_tasks_created_total")
                    || line.starts_with("aircd_tasks_completed_total")
                    || line.starts_with("aircd_task_transitions_total")
            })
            .map(str::to_string)
            .collect()
    }

    fn new(parent: Option<&str>, optional: bool, condition: Option<TaskCondition>) -> NewTask {
        NewTask {
            channel: "#one".into(),
            title: "Branch".into(),
            parent_task_id: parent.map(str::to_string),
            optional,
            condition,
            ..Default::default()
        }
    }

    #[test]
    fn conditional_audit_failure_rolls_back_completion_output_decision_and_metrics() -> Result<()> {
        let db = crate::store::open(":memory:")?;
        let store = TaskStore::new(db.clone(), 60);
        let parent = store.create_full(new(None, false, None))?;
        let child = store.create_full(new(
            Some(&parent.id),
            false,
            Some(TaskCondition::ParentOutputExists {
                pointer: "/ok".into(),
            }),
        ))?;
        let parent = store.claim(&parent.id, "worker")?;
        db.write()?.execute_batch("CREATE TRIGGER reject_decision BEFORE INSERT ON task_events WHEN NEW.action = 'condition_activate' BEGIN SELECT RAISE(ABORT, 'forced failure'); END;")?;
        let counters = task_counters(&store);
        assert!(store
            .done_with_output(&parent.id, "worker", Some(&serde_json::json!({"ok":true})))
            .is_err());
        assert_eq!(store.get(&parent.id)?.unwrap(), parent);
        assert_eq!(store.get(&child.id)?.unwrap(), child);
        assert_eq!(task_counters(&store), counters);
        assert!(store.events(&parent.id)?.is_empty());
        db.write()?.execute("DROP TRIGGER reject_decision", [])?;
        let update =
            store.done_with_output(&parent.id, "worker", Some(&serde_json::json!({"ok":true})))?;
        assert_eq!(update.task.output, Some(serde_json::json!({"ok":true})));
        assert_eq!(update.cascaded.len(), 1);
        assert_eq!(store.get(&child.id)?.unwrap().status, "open");
        assert!(store
            .done_with_output(&parent.id, "worker", Some(&serde_json::json!({"ok":false})))
            .is_err());
        Ok(())
    }

    #[test]
    fn conditional_restart_preserves_enabled_gate_pauses_and_immutable_output() -> Result<()> {
        let path =
            std::env::temp_dir().join(format!("aircd-conditions-{}.sqlite3", Uuid::new_v4()));
        let (parent_id, child_id);
        {
            let db = crate::store::open(&path)?;
            let store = TaskStore::new(db, 60);
            parent_id = store
                .create_full(NewTask {
                    assignee: Some("lead".into()),
                    ..new(None, false, None)
                })?
                .id;
            child_id = store
                .create_full(new(
                    Some(&parent_id),
                    true,
                    Some(TaskCondition::ParentOutputEquals {
                        pointer: "/route".into(),
                        value: serde_json::json!("go"),
                    }),
                ))?
                .id;
            assert_eq!(
                store.enable_optional(&child_id, "lead")?.task.status,
                "dormant"
            );
            store.ask(&parent_id, "lead", "Persist pause")?;
        }
        {
            let db = crate::store::open(&path)?;
            let store = TaskStore::new(db, 60);
            assert!(store.get(&child_id)?.unwrap().optional_enabled);
            assert_eq!(store.get(&child_id)?.unwrap().status, "blocked");
            store.answer(&parent_id, "pm", "Resume")?;
            assert_eq!(store.get(&child_id)?.unwrap().status, "dormant");
            let current = store.get(&parent_id)?.unwrap();
            store.set_status_batch(
                &[PreparedTaskStatus {
                    current,
                    to: "merged".into(),
                    note: None,
                    branch: None,
                    round: None,
                    output: Some(serde_json::json!({"route":"go"})),
                }],
                "pm",
            )?;
        }
        {
            let db = crate::store::open(&path)?;
            let store = TaskStore::new(db, 60);
            assert_eq!(
                store.get(&parent_id)?.unwrap().output,
                Some(serde_json::json!({"route":"go"}))
            );
            assert_eq!(store.get(&child_id)?.unwrap().status, "open");
            assert!(store.get(&child_id)?.unwrap().optional_enabled);
        }
        std::fs::remove_file(path)?;
        Ok(())
    }

    #[test]
    fn conditional_schema_migration_preserves_legacy_rows_and_is_idempotent() -> Result<()> {
        let path = std::env::temp_dir().join(format!(
            "aircd-condition-migration-{}.sqlite3",
            Uuid::new_v4()
        ));
        let legacy;
        {
            let db = crate::store::open(&path)?;
            let store = TaskStore::new(db.clone(), 60);
            legacy = store.create_full(new(None, false, None))?;
            for column in [
                "condition_json",
                "optional",
                "optional_enabled",
                "output_json",
            ] {
                db.write()?
                    .execute(&format!("ALTER TABLE tasks DROP COLUMN {column}"), [])?;
            }
        }
        for _ in 0..2 {
            let db = crate::store::open(&path)?;
            let store = TaskStore::new(db, 60);
            assert_eq!(store.get(&legacy.id)?.unwrap(), legacy);
        }
        std::fs::remove_file(path)?;
        Ok(())
    }

    #[test]
    fn conditional_optional_enable_audit_failure_rolls_back_flag_and_nested_activation(
    ) -> Result<()> {
        let db = crate::store::open(":memory:")?;
        let store = TaskStore::new(db.clone(), 60);
        let parent = store.create_full(new(None, false, None))?;
        let child = store.create_full(new(Some(&parent.id), true, None))?;
        let nested = store.create_full(new(Some(&child.id), false, None))?;
        assert_eq!(nested.status, "dormant");
        db.write()?.execute_batch("CREATE TRIGGER reject_enable BEFORE INSERT ON task_events WHEN NEW.action = 'condition_activate' BEGIN SELECT RAISE(ABORT, 'forced failure'); END;")?;
        let counters = task_counters(&store);
        assert!(store.enable_optional(&child.id, "pm").is_err());
        assert_eq!(store.get(&child.id)?.unwrap(), child);
        assert_eq!(store.get(&nested.id)?.unwrap(), nested);
        assert!(store.events(&child.id)?.is_empty());
        assert_eq!(task_counters(&store), counters);
        db.write()?.execute("DROP TRIGGER reject_enable", [])?;
        let update = store.enable_optional(&child.id, "pm")?;
        assert_eq!(update.cascaded.len(), 2);
        assert_eq!(store.get(&nested.id)?.unwrap().status, "open");
        Ok(())
    }
}
