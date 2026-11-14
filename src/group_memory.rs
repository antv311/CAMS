//! Durable group working context. Authorization and edits share a DB snapshot.
use super::{group_name, GroupStore, GroupStoreError};
use crate::history::unix_timestamp;
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::Serialize;

pub const MAX_GROUP_MEMORY_CHARS: usize = 16_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GroupMemory {
    pub group: String,
    pub kind: String,
    pub body: String,
    /// Zero means this document has never been written.
    pub revision: i64,
    pub updated_at: Option<i64>,
    pub updated_by: Option<String>,
}

#[derive(Debug)]
pub enum GroupMemoryError {
    Forbidden,
    Conflict,
    Invalid(String),
}

impl std::fmt::Display for GroupMemoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Forbidden => write!(f, "group memory requires a current named group member"),
            Self::Conflict => write!(
                f,
                "group memory revision changed; read it again before editing"
            ),
            Self::Invalid(message) => f.write_str(message),
        }
    }
}
impl std::error::Error for GroupMemoryError {}

impl GroupStore {
    fn authorize_memory(db: &Connection, name: &str, nick: &str) -> Result<()> {
        let group = Self::get_locked(db, name)?
            .ok_or_else(|| GroupStoreError::GroupNotFound(name.to_string()))?;
        let member = std::iter::once(&group.definition.lead)
            .chain(&group.definition.workers)
            .any(|seat| {
                seat.nick
                    .as_deref()
                    .is_some_and(|bound| bound.trim_start_matches('@').eq_ignore_ascii_case(nick))
            });
        if !member {
            return Err(GroupMemoryError::Forbidden.into());
        }
        Ok(())
    }

    pub fn memory(&self, name: &str, kind: &str, nick: &str) -> Result<GroupMemory> {
        let name = group_name(name)?;
        validate_kind(kind)?;
        let db = self.db.read()?;
        let tx = db.unchecked_transaction()?;
        Self::authorize_memory(&tx, name, nick)?;
        let memory = load(&tx, name, kind)?;
        tx.commit()?;
        Ok(memory)
    }

    /// Append under the writer lock, or replace using a required revision guard.
    /// The roster check runs inside the same immediate transaction as the edit.
    pub fn write_memory(
        &self,
        name: &str,
        kind: &str,
        nick: &str,
        text: &str,
        append: bool,
        expected_revision: Option<i64>,
    ) -> Result<GroupMemory> {
        let name = group_name(name)?;
        validate_kind(kind)?;
        if !append && expected_revision.is_none() {
            return invalid("replacing memory requires expected_revision (read it first)");
        }
        if expected_revision.is_some_and(|revision| revision < 0) {
            return invalid("expected_revision must be non-negative");
        }
        if text.contains('\0') {
            return invalid("group memory must not contain NUL");
        }
        let mut db = self.db.write()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::authorize_memory(&tx, name, nick)?;
        let current = load(&tx, name, kind)?;
        if expected_revision.is_some_and(|revision| revision != current.revision) {
            return Err(GroupMemoryError::Conflict.into());
        }
        let body = if append {
            let addition = text.trim();
            if addition.is_empty() {
                return invalid("nothing to add");
            }
            if current.body.is_empty() {
                addition.to_string()
            } else {
                format!("{}\n{addition}", current.body.trim_end())
            }
        } else {
            text.to_string()
        };
        if body.chars().count() > MAX_GROUP_MEMORY_CHARS {
            return invalid(&format!(
                "group memory would exceed {MAX_GROUP_MEMORY_CHARS} characters; replace it with shorter context"
            ));
        }
        let memory = GroupMemory {
            group: name.to_string(),
            kind: kind.to_string(),
            body,
            revision: current.revision + 1,
            updated_at: Some(unix_timestamp()),
            updated_by: Some(nick.to_string()),
        };
        tx.execute(
            "INSERT INTO group_memory (group_name, kind, body, revision, updated_at, updated_by)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(group_name, kind) DO UPDATE SET body = excluded.body,
               revision = excluded.revision, updated_at = excluded.updated_at,
               updated_by = excluded.updated_by",
            params![
                name,
                kind,
                memory.body,
                memory.revision,
                memory.updated_at,
                nick
            ],
        )
        .context("save group memory")?;
        tx.commit()?;
        Ok(memory)
    }
}

fn invalid<T>(message: &str) -> Result<T> {
    Err(GroupMemoryError::Invalid(message.to_string()).into())
}

fn validate_kind(kind: &str) -> Result<()> {
    if !matches!(kind, "notes" | "scratchpad") {
        return invalid("memory kind must be notes or scratchpad");
    }
    Ok(())
}

fn load(db: &Connection, name: &str, kind: &str) -> Result<GroupMemory> {
    let saved = db
        .query_row(
            "SELECT body, revision, updated_at, updated_by FROM group_memory
         WHERE group_name = ?1 AND kind = ?2",
            params![name, kind],
            |row| {
                Ok(GroupMemory {
                    group: name.to_string(),
                    kind: kind.to_string(),
                    body: row.get(0)?,
                    revision: row.get(1)?,
                    updated_at: row.get(2)?,
                    updated_by: row.get(3)?,
                })
            },
        )
        .optional()
        .context("load group memory")?;
    Ok(saved.unwrap_or_else(|| GroupMemory {
        group: name.to_string(),
        kind: kind.to_string(),
        body: String::new(),
        revision: 0,
        updated_at: None,
        updated_by: None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{self, GroupInput, GroupMembershipUpdate};

    fn definition(name: &str) -> GroupInput {
        serde_json::from_value(serde_json::json!({
            "name": name,
            "lead": {"template": "example", "model": "lead", "nick": "@Lead"},
            "workers": [{"template": "example", "model": "worker", "nick": "worker"}]
        }))
        .unwrap()
    }

    #[test]
    fn memory_scopes_documents_and_preserves_context_on_group_updates() -> Result<()> {
        let groups = GroupStore::new(store::open(":memory:")?);
        groups.upsert(definition("a"))?;
        groups.upsert(definition("b"))?;
        let initial = groups.memory("a", "notes", "LEAD")?;
        assert_eq!(
            (initial.body.as_str(), initial.revision, initial.updated_at),
            ("", 0, None)
        );
        let saved = groups.write_memory("a", "notes", "worker", " context\n", false, Some(0))?;
        assert_eq!(saved.updated_by.as_deref(), Some("worker"));
        assert!(saved.updated_at.is_some());
        groups.write_memory("a", "scratchpad", "Lead", "temporary", true, None)?;
        groups.upsert(definition("a"))?;
        groups.update_membership(
            "a",
            GroupMembershipUpdate {
                lead: Some(definition("a").lead),
                ..Default::default()
            },
        )?;
        assert_eq!(groups.memory("a", "notes", "Lead")?, saved);
        assert_eq!(groups.memory("b", "notes", "worker")?.revision, 0);
        assert_eq!(
            groups.memory("a", "scratchpad", "worker")?.body,
            "temporary"
        );
        assert!(groups
            .memory("a", "notes", "outsider")
            .unwrap_err()
            .is::<GroupMemoryError>());
        assert!(groups
            .memory("missing", "notes", "Lead")
            .unwrap_err()
            .is::<GroupStoreError>());
        groups.delete("a")?;
        groups.upsert(definition("a"))?;
        assert_eq!(groups.memory("a", "notes", "worker")?.revision, 0);
        Ok(())
    }

    #[test]
    fn memory_rejects_stale_or_invalid_writes_without_changes() -> Result<()> {
        let groups = GroupStore::new(store::open(":memory:")?);
        groups.upsert(definition("a"))?;
        let original = groups.write_memory("a", "notes", "Lead", "keep", false, Some(0))?;
        for (text, append, revision) in [
            ("overwrite", false, Some(0)),
            ("overwrite", false, None),
            ("bad", true, Some(-1)),
            ("  ", true, None),
            ("nul\0", false, Some(1)),
        ] {
            assert!(groups
                .write_memory("a", "notes", "worker", text, append, revision)
                .is_err());
            assert_eq!(groups.memory("a", "notes", "Lead")?, original);
        }
        assert!(groups
            .write_memory("a", "other", "Lead", "bad", true, None)
            .is_err());
        assert!(groups
            .write_memory("a", "notes", "outsider", "bad", true, None)
            .is_err());
        let max = "é".repeat(MAX_GROUP_MEMORY_CHARS);
        groups.write_memory("a", "notes", "worker", &max, false, Some(1))?;
        assert!(groups
            .write_memory("a", "notes", "worker", "x", true, None)
            .is_err());
        let cleared = groups.write_memory("a", "notes", "Lead", "", false, Some(2))?;
        assert_eq!((cleared.body.as_str(), cleared.revision), ("", 3));
        let stale = groups
            .write_memory("a", "notes", "worker", "stale", false, Some(2))
            .unwrap_err();
        assert!(matches!(
            stale.downcast_ref::<GroupMemoryError>(),
            Some(GroupMemoryError::Conflict)
        ));
        Ok(())
    }

    #[test]
    fn memory_survives_reopen_and_migrates_existing_groups() -> Result<()> {
        let path =
            std::env::temp_dir().join(format!("aircd-memory-{}.sqlite3", uuid::Uuid::new_v4()));
        {
            let db = store::open(&path)?;
            GroupStore::new(db.clone()).upsert(definition("team"))?;
            // Simulate a hub created before the memory migration.
            db.write()?.execute("DROP TABLE group_memory", [])?;
        }
        let saved;
        {
            let groups = GroupStore::new(store::open(&path)?);
            assert_eq!(groups.memory("team", "notes", "Lead")?.revision, 0);
            saved = groups.write_memory(
                "team",
                "notes",
                "Lead",
                "persistent é {task}",
                false,
                Some(0),
            )?;
        }
        {
            let groups = GroupStore::new(store::open(&path)?);
            assert_eq!(groups.memory("team", "notes", "worker")?, saved);
        }
        std::fs::remove_file(path)?;
        Ok(())
    }

    #[test]
    fn memory_concurrent_connections_preserve_appends_and_reject_competing_replacements(
    ) -> Result<()> {
        let path =
            std::env::temp_dir().join(format!("aircd-memory-{}.sqlite3", uuid::Uuid::new_v4()));
        {
            let first = GroupStore::new(store::open(&path)?);
            let second = GroupStore::new(store::open(&path)?);
            first.upsert(definition("team"))?;
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let threads: Vec<_> = [first.clone(), second.clone()]
                .into_iter()
                .enumerate()
                .map(|(i, store)| {
                    let barrier = barrier.clone();
                    std::thread::spawn(move || -> Result<()> {
                        barrier.wait();
                        for j in 0..20 {
                            store.write_memory(
                                "team",
                                "notes",
                                "worker",
                                &format!("{i}:{j}"),
                                true,
                                None,
                            )?;
                        }
                        Ok(())
                    })
                })
                .collect();
            for thread in threads {
                thread.join().unwrap()?;
            }
            let memory = first.memory("team", "notes", "Lead")?;
            assert_eq!(memory.revision, 40);
            let lines: std::collections::HashSet<_> = memory.body.lines().collect();
            assert_eq!(lines.len(), 40);
            for i in 0..2 {
                for j in 0..20 {
                    assert!(lines.contains(format!("{i}:{j}").as_str()));
                }
            }
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let threads: Vec<_> = [first.clone(), second]
                .into_iter()
                .map(|store| {
                    let barrier = barrier.clone();
                    std::thread::spawn(move || {
                        barrier.wait();
                        store.write_memory("team", "notes", "Lead", "replacement", false, Some(40))
                    })
                })
                .collect();
            let outcomes: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
            assert_eq!(outcomes.iter().filter(|r| r.is_ok()).count(), 1);
            assert!(outcomes
                .iter()
                .filter_map(|r| r.as_ref().err())
                .all(|e| matches!(
                    e.downcast_ref::<GroupMemoryError>(),
                    Some(GroupMemoryError::Conflict)
                )));
            assert_eq!(first.memory("team", "notes", "worker")?.revision, 41);
        }
        std::fs::remove_file(path)?;
        Ok(())
    }
}
