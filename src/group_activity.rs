//! Opt-in group task takeover. Policy, eligibility, task writes and both audit
//! records are checked under one SQLite write transaction.
use super::{group_name, Group, GroupStore, GroupStoreError};
use crate::history::unix_timestamp;
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::Serialize;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug, Serialize)]
pub struct GroupReassignment {
    pub id: i64,
    pub group: String,
    pub task_id: String,
    pub old_assignee: String,
    pub new_assignee: String,
    pub created_at: i64,
    pub note: String,
}

pub(super) fn migrate(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS principal_activity (
           principal_id TEXT PRIMARY KEY NOT NULL,
           last_activity_at INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS group_reassignments (
           id INTEGER PRIMARY KEY AUTOINCREMENT,
           group_name TEXT NOT NULL REFERENCES groups(name) ON DELETE CASCADE,
           task_id TEXT NOT NULL,
           old_assignee TEXT NOT NULL,
           new_assignee TEXT NOT NULL,
           created_at INTEGER NOT NULL,
           note TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_group_reassignments_group
           ON group_reassignments(group_name, id);
         CREATE TRIGGER IF NOT EXISTS activity_on_chat AFTER INSERT ON messages
         WHEN EXISTS(SELECT 1 FROM principals WHERE id = NEW.sender_id)
         BEGIN
           INSERT INTO principal_activity VALUES (NEW.sender_id, NEW.created_at)
           ON CONFLICT(principal_id) DO UPDATE SET
             last_activity_at = MAX(last_activity_at, excluded.last_activity_at);
         END;
         CREATE TRIGGER IF NOT EXISTS activity_on_task_update AFTER INSERT ON task_events
         WHEN EXISTS(SELECT 1 FROM principals WHERE id = NEW.actor_id)
         BEGIN
           INSERT INTO principal_activity VALUES (NEW.actor_id, NEW.created_at)
           ON CONFLICT(principal_id) DO UPDATE SET
             last_activity_at = MAX(last_activity_at, excluded.last_activity_at);
         END;",
    )?;
    Ok(())
}

impl GroupStore {
    /// Heartbeats are server-timed and authenticated as the caller. Presence is
    /// account-wide, just like a chat or task update in any project.
    pub fn heartbeat(&self, name: &str, nick: &str, principal_id: &str) -> Result<i64> {
        let name = group_name(name)?;
        let mut db = self.db.write()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::authorize_memory(&tx, name, nick)?;
        let now = unix_timestamp();
        tx.execute(
            "INSERT INTO principal_activity VALUES (?1, ?2)
             ON CONFLICT(principal_id) DO UPDATE SET
               last_activity_at = MAX(last_activity_at, excluded.last_activity_at)",
            params![principal_id, now],
        )?;
        tx.commit()?;
        Ok(now)
    }

    pub fn reassignment_events(&self, name: &str, nick: &str) -> Result<Vec<GroupReassignment>> {
        let name = group_name(name)?;
        let db = self.db.read()?;
        let tx = db.unchecked_transaction()?;
        Self::authorize_memory(&tx, name, nick)?;
        let events = {
            let mut statement = tx.prepare(
                "SELECT id, group_name, task_id, old_assignee, new_assignee, created_at, note
                 FROM group_reassignments WHERE group_name = ?1 ORDER BY id DESC LIMIT 100",
            )?;
            let rows = statement.query_map([name], |row| {
                Ok(GroupReassignment {
                    id: row.get(0)?,
                    group: row.get(1)?,
                    task_id: row.get(2)?,
                    old_assignee: row.get(3)?,
                    new_assignee: row.get(4)?,
                    created_at: row.get(5)?,
                    note: row.get(6)?,
                })
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        tx.commit()?;
        Ok(events)
    }

    pub fn reassign_inactive(&self, now: i64) -> Result<Vec<GroupReassignment>> {
        let enabled: bool = self.db.read()?.query_row(
            "SELECT EXISTS(SELECT 1 FROM groups WHERE inactivity_timeout_secs > 0)",
            [],
            |row| row.get(0),
        )?;
        if !enabled {
            return Ok(Vec::new());
        }
        let mut db = self.db.write()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let names = {
            let mut statement = tx.prepare("SELECT name FROM groups ORDER BY name")?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let groups = names
            .iter()
            .map(|name| {
                Self::get_locked(&tx, name)?
                    .ok_or_else(|| GroupStoreError::GroupNotFound(name.clone()).into())
            })
            .collect::<Result<Vec<Group>>>()?;
        // Ambiguous roles fail closed, including overlaps with disabled groups
        // and duplicate bindings within a single group.
        let mut bindings: HashMap<(String, String), usize> = HashMap::new();
        for group in &groups {
            for member in std::iter::once(&group.definition.lead).chain(&group.definition.workers) {
                if let Some(id) = resolve_member(&tx, member.nick.as_deref())? {
                    for project in &group.projects {
                        *bindings.entry((project.clone(), id.clone())).or_default() += 1;
                    }
                }
            }
        }
        let mut events = Vec::new();
        for group in groups {
            let Some(timeout) = group.definition.inactivity_timeout_secs.filter(|s| *s > 0) else {
                continue;
            };
            let cutoff = now.saturating_sub(timeout);
            // Enabling/changing a definition grants a full timeout to old tasks.
            if group.updated_at > cutoff {
                continue;
            }
            let workers = group
                .definition
                .workers
                .iter()
                .filter_map(|member| member.nick.as_deref())
                .map(|nick| resolve_member(&tx, Some(nick)))
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .flatten()
                .collect::<HashSet<_>>();
            let mut workers: Vec<_> = workers.into_iter().collect();
            workers.sort();
            for project in &group.projects {
                let eligible: Vec<_> = workers
                    .iter()
                    .filter(|id| bindings.get(&(project.clone(), (*id).clone())) == Some(&1))
                    .cloned()
                    .collect();
                let stalled = {
                    let mut statement = tx.prepare(
                        "SELECT t.id, t.assignee, t.status FROM tasks t
                         JOIN project_roles p ON p.channel = t.channel AND p.principal_id = t.assignee
                         LEFT JOIN principal_activity a ON a.principal_id = t.assignee
                         WHERE t.channel = ?1 AND t.status IN ('assigned', 'in_progress', 'changes_requested')
                           AND t.updated_at <= ?2 AND COALESCE(a.last_activity_at, 0) <= ?2
                         ORDER BY t.created_at, t.id",
                    )?;
                    let rows = statement.query_map(params![project, cutoff], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    })?;
                    rows.collect::<rusqlite::Result<Vec<_>>>()?
                };
                let stalled_workers: HashSet<_> =
                    stalled.iter().map(|(_, id, _)| id.clone()).collect();
                for (task_id, old, status) in stalled {
                    if !eligible.contains(&old) {
                        continue;
                    }
                    let mut replacement = None;
                    for id in &eligible {
                        if id == &old || stalled_workers.contains(id) {
                            continue;
                        }
                        let available: bool = tx.query_row(
                            "SELECT EXISTS(SELECT 1 FROM project_roles p
                               JOIN wake_configs w ON w.principal_id = p.principal_id AND w.enabled = 1
                               WHERE p.channel = ?1 AND p.principal_id = ?2)
                             AND NOT EXISTS(SELECT 1 FROM tasks WHERE channel = ?1
                               AND (assignee = ?2 OR claimed_by = ?2)
                               AND status NOT IN ('done', 'merged', 'cancelled'))",
                            params![project, id], |row| row.get(0),
                        )?;
                        if available {
                            replacement = Some(id.clone());
                            break;
                        }
                    }
                    let Some(new) = replacement else { continue };
                    let next = if status == "changes_requested" {
                        "changes_requested"
                    } else {
                        "assigned"
                    };
                    let note = format!(
                        "group={} inactivity_timeout_secs={timeout} old_assignee={old} new_assignee={new}",
                        group.definition.name,
                    );
                    tx.execute(
                        "UPDATE tasks SET assignee = ?2, status = ?3, updated_at = ?4,
                           claimed_by = NULL, lease_expires_at = NULL, review_round_json = NULL WHERE id = ?1",
                        params![task_id, new, next, now],
                    )?;
                    tx.execute(
                        "INSERT INTO task_events(task_id, actor_id, action, from_status, to_status, note, created_at)
                         VALUES (?1, 'aircd', 'auto_reassign', ?2, ?3, ?4, ?5)",
                        params![task_id, status, next, note, now],
                    )?;
                    tx.execute(
                        "INSERT INTO group_reassignments(group_name, task_id, old_assignee, new_assignee, created_at, note)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        params![group.definition.name, task_id, old, new, now, note],
                    )?;
                    events.push(GroupReassignment {
                        id: tx.last_insert_rowid(),
                        group: group.definition.name.clone(),
                        task_id,
                        old_assignee: old,
                        new_assignee: new,
                        created_at: now,
                        note,
                    });
                }
            }
        }
        tx.commit()?;
        Ok(events)
    }
}

fn resolve_member(db: &Connection, nick: Option<&str>) -> Result<Option<String>> {
    let Some(nick) = nick else { return Ok(None) };
    Ok(db
        .query_row(
            "SELECT id FROM principals WHERE lower(nick) = lower(?1)",
            [nick.trim_start_matches('@')],
            |row| row.get(0),
        )
        .optional()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::Server;
    use crate::store::{self, GroupInput};
    use crate::tasks::{NewTask, ALL_STATUSES};
    use crate::wake::WakeConfigInput;

    fn setup(db: super::super::SharedConnection) -> Result<Server> {
        let server = Server::new(db);
        for id in ["human", "agent-1", "agent-2", "agent-3"] {
            if server.auth().resolve_by_name(id)?.is_none() {
                server
                    .auth()
                    .register_principal(id, Some("builder"), None)?;
            }
        }
        for project in ["#one", "#two"] {
            server
                .projects()
                .create(project, "Project", None, "human")?;
            for id in ["human", "agent-1", "agent-2", "agent-3"] {
                server.projects().set_role(project, id, "builder")?;
            }
        }
        let input: GroupInput = serde_json::from_value(serde_json::json!({
            "name": "team", "inactivity_timeout_secs": 60,
            "lead": {"template": "x", "model": "m", "nick": "human"},
            "workers": [
                {"template": "x", "model": "m", "nick": "@AGENT-1"},
                {"template": "x", "model": "m", "nick": "agent-2"},
                {"template": "x", "model": "m", "nick": "agent-3"},
                {"template": "x", "model": "m", "count": 10}
            ]
        }))?;
        server.groups().upsert(input)?;
        server.groups().attach("team", "#one")?;
        for id in ["human", "agent-1", "agent-2", "agent-3"] {
            server.wakes().upsert(
                id,
                WakeConfigInput {
                    command: "never-launched-by-store-tests".into(),
                    ..Default::default()
                },
            )?;
        }
        server
            .db()
            .write()?
            .execute("UPDATE groups SET updated_at = 100", [])?;
        Ok(server)
    }

    fn task(server: &Server, status: &str, assignee: &str, project: &str) -> Result<String> {
        let task = server.tasks().create_full(NewTask {
            channel: project.into(),
            title: "Stalled work".into(),
            assignee: Some(assignee.into()),
            body: Some("Context to preserve".into()),
            branch: Some("feat/existing".into()),
            reviewer: Some("human".into()),
            ..Default::default()
        })?;
        server.db().write()?.execute(
            "UPDATE tasks SET status = ?2, created_at = 100, updated_at = 100 WHERE id = ?1",
            params![task.id, status],
        )?;
        Ok(task.id)
    }

    #[test]
    fn inactivity_only_targets_approved_states_and_preserves_context_and_roster() -> Result<()> {
        for status in ALL_STATUSES {
            let server = setup(store::open(":memory:")?)?;
            let before = server.groups().get("team")?.unwrap();
            let id = task(&server, status, "agent-1", "#one")?;
            let events = server.groups().reassign_inactive(1000)?;
            let expected = matches!(*status, "assigned" | "in_progress" | "changes_requested");
            assert_eq!(events.len(), usize::from(expected), "{status}");
            let current = server.tasks().get(&id)?.unwrap();
            if expected {
                assert_eq!(current.assignee.as_deref(), Some("agent-2"));
                assert_eq!(
                    current.status,
                    if *status == "changes_requested" {
                        "changes_requested"
                    } else {
                        "assigned"
                    }
                );
                assert_eq!(current.body.as_deref(), Some("Context to preserve"));
                assert_eq!(current.branch.as_deref(), Some("feat/existing"));
                assert_eq!(current.reviewer.as_deref(), Some("human"));
                let history = server.tasks().events(&id)?;
                assert_eq!(history.len(), 1);
                assert_eq!(history[0].action, "auto_reassign");
                assert!(history[0]
                    .note
                    .as_deref()
                    .unwrap()
                    .contains("old_assignee=agent-1"));
                assert_eq!(
                    server.groups().reassignment_events("team", "human")?.len(),
                    1
                );
                assert!(server.groups().reassign_inactive(1000)?.is_empty());
            } else {
                assert_eq!(current.assignee.as_deref(), Some("agent-1"));
                assert!(server.tasks().events(&id)?.is_empty());
            }
            assert_eq!(server.groups().get("team")?.unwrap(), before);
        }
        Ok(())
    }

    #[test]
    fn inactivity_disabled_default_validation_and_enable_grace() -> Result<()> {
        let server = setup(store::open(":memory:")?)?;
        task(&server, "assigned", "agent-1", "#one")?;
        let mut definition = server.groups().get("team")?.unwrap().definition;
        for invalid in [0, -1] {
            definition.inactivity_timeout_secs = Some(invalid);
            assert!(server.groups().upsert(definition.clone()).is_err());
        }
        definition.inactivity_timeout_secs = None;
        server.groups().upsert(definition.clone())?;
        assert!(server
            .groups()
            .reassign_inactive(unix_timestamp() + 100)?
            .is_empty());
        definition.inactivity_timeout_secs = Some(60);
        let group = server.groups().upsert(definition)?;
        assert!(server
            .groups()
            .reassign_inactive(group.updated_at + 59)?
            .is_empty());
        assert_eq!(
            server
                .groups()
                .reassign_inactive(group.updated_at + 60)?
                .len(),
            1
        );
        let old: GroupInput = serde_json::from_value(serde_json::json!({
            "name": "old", "lead": {"template":"x", "model":"m"},
            "workers": [{"template":"x", "model":"m"}]
        }))?;
        assert_eq!(
            server
                .groups()
                .upsert(old)?
                .definition
                .inactivity_timeout_secs,
            None
        );
        Ok(())
    }

    #[test]
    fn inactivity_activity_from_chat_status_and_heartbeat_prevents_takeover() -> Result<()> {
        for activity in ["chat", "status", "heartbeat"] {
            let server = setup(store::open(":memory:")?)?;
            let id = task(&server, "assigned", "agent-1", "#one")?;
            match activity {
                "chat" => {
                    server
                        .history()
                        .append("#two", "agent-1", "Still working")?;
                }
                "status" => {
                    server.tasks().set_status(
                        &id,
                        "assigned",
                        "in_progress",
                        "agent-1",
                        None,
                        None,
                    )?;
                }
                _ => {
                    server.groups().heartbeat("team", "agent-1", "agent-1")?;
                }
            }
            // A status updates both the task and actor; isolate the actor check.
            server
                .db()
                .write()?
                .execute("UPDATE tasks SET updated_at = 100", [])?;
            assert!(
                server
                    .groups()
                    .reassign_inactive(unix_timestamp() + 30)?
                    .is_empty(),
                "{activity}"
            );
            assert_eq!(
                server
                    .groups()
                    .reassign_inactive(unix_timestamp() + 61)?
                    .len(),
                1,
                "{activity}"
            );
        }
        let server = setup(store::open(":memory:")?)?;
        let id = task(&server, "assigned", "agent-1", "#one")?;
        server
            .db()
            .write()?
            .execute("UPDATE tasks SET updated_at = 950 WHERE id = ?1", [&id])?;
        assert!(server.groups().reassign_inactive(1000)?.is_empty());
        assert_eq!(server.groups().reassign_inactive(1010)?.len(), 1);
        Ok(())
    }

    #[test]
    fn inactivity_requires_idle_enabled_current_named_workers_in_same_project() -> Result<()> {
        for unavailable in ["disabled", "not_member", "busy", "claimed", "ambiguous"] {
            let server = setup(store::open(":memory:")?)?;
            let id = task(&server, "assigned", "agent-1", "#one")?;
            match unavailable {
                "disabled" => {
                    server.wakes().set_enabled("agent-2", false)?;
                }
                "not_member" => {
                    server.db().write()?.execute("DELETE FROM project_roles WHERE principal_id = 'agent-2' AND channel = '#one'", [])?;
                }
                "busy" => {
                    task(&server, "blocked", "agent-2", "#one")?;
                }
                "claimed" => {
                    let claimed = task(&server, "claimed", "human", "#one")?;
                    server.db().write()?.execute(
                        "UPDATE tasks SET claimed_by = 'agent-2' WHERE id = ?1",
                        [&claimed],
                    )?;
                }
                _ => {
                    let mut other = server.groups().get("team")?.unwrap().definition;
                    other.name = "other".into();
                    other.inactivity_timeout_secs = None;
                    other
                        .workers
                        .retain(|m| m.nick.as_deref() == Some("agent-2"));
                    server.groups().upsert(other)?;
                    server.groups().attach("other", "#one")?;
                }
            }
            assert_eq!(
                server.groups().reassign_inactive(1000)?.len(),
                1,
                "{unavailable}"
            );
            assert_eq!(
                server.tasks().get(&id)?.unwrap().assignee.as_deref(),
                Some("agent-3")
            );
        }
        // A task in another project does not consume this project's idle slot.
        let server = setup(store::open(":memory:")?)?;
        task(&server, "review", "agent-2", "#two")?;
        let id = task(&server, "assigned", "agent-1", "#one")?;
        assert_eq!(server.groups().reassign_inactive(1000)?.len(), 1);
        assert_eq!(
            server.tasks().get(&id)?.unwrap().assignee.as_deref(),
            Some("agent-2")
        );
        Ok(())
    }

    #[test]
    fn inactivity_does_not_replace_leads_or_reuse_stalled_workers_or_busy_peers() -> Result<()> {
        let server = setup(store::open(":memory:")?)?;
        task(&server, "assigned", "human", "#one")?;
        assert!(server.groups().reassign_inactive(1000)?.is_empty());
        for id in ["agent-1", "agent-2", "agent-3"] {
            task(&server, "in_progress", id, "#one")?;
        }
        assert!(server.groups().reassign_inactive(1000)?.is_empty());
        Ok(())
    }

    #[test]
    fn inactivity_audit_failure_rolls_back_task_and_both_logs() -> Result<()> {
        let server = setup(store::open(":memory:")?)?;
        let id = task(&server, "in_progress", "agent-1", "#one")?;
        server.db().write()?.execute_batch(
            "CREATE TRIGGER reject_group_log BEFORE INSERT ON group_reassignments BEGIN SELECT RAISE(ABORT, 'test audit failure'); END;"
        )?;
        assert!(server.groups().reassign_inactive(1000).is_err());
        let task = server.tasks().get(&id)?.unwrap();
        assert_eq!(task.assignee.as_deref(), Some("agent-1"));
        assert_eq!(task.status, "in_progress");
        assert!(server.tasks().events(&id)?.is_empty());
        assert!(server
            .groups()
            .reassignment_events("team", "human")?
            .is_empty());
        Ok(())
    }

    #[test]
    fn inactivity_concurrent_sweeps_reopen_and_group_deletion() -> Result<()> {
        let path =
            std::env::temp_dir().join(format!("aircd-inactivity-{}.sqlite3", uuid::Uuid::new_v4()));
        let server = setup(store::open(&path)?)?;
        let id = task(&server, "assigned", "agent-1", "#one")?;
        let other = GroupStore::new(store::open(&path)?);
        let groups = server.groups().clone();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let b = barrier.clone();
        let a = std::thread::spawn(move || {
            b.wait();
            groups.reassign_inactive(1000)
        });
        let b = std::thread::spawn(move || {
            barrier.wait();
            other.reassign_inactive(1000)
        });
        assert_eq!(a.join().unwrap()?.len() + b.join().unwrap()?.len(), 1);
        assert_eq!(server.tasks().events(&id)?.len(), 1);
        drop(server);
        let reopened = GroupStore::new(store::open(&path)?);
        assert_eq!(
            reopened
                .get("team")?
                .unwrap()
                .definition
                .inactivity_timeout_secs,
            Some(60)
        );
        assert_eq!(reopened.reassignment_events("team", "human")?.len(), 1);
        assert!(reopened.delete("team")?);
        assert_eq!(
            reopened
                .db
                .read()?
                .query_row("SELECT COUNT(*) FROM group_reassignments", [], |r| r
                    .get::<_, i64>(0))?,
            0
        );
        drop(reopened);
        let _ = std::fs::remove_file(path);
        Ok(())
    }
}
