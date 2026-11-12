//! Projects: a channel that owns tasks, a repo and its own roles.
//!
//! An account can belong to any number of projects. What keeps work in the
//! right place is the task, which names its project channel, not the account.

use anyhow::{anyhow, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::sync::{Arc, Mutex};

use crate::history::unix_timestamp;

/// Project roles that may assign work and decide reviews.
pub const MANAGER_ROLES: &[&str] = &["leader", "coordinator"];

/// `#general` is for conversation only; it can never be a project.
pub const CHAT_ONLY_CHANNEL: &str = "#general";

#[derive(Clone, Debug, Serialize)]
pub struct Project {
    pub channel: String,
    pub name: String,
    pub description: Option<String>,
    pub created_by: String,
    pub created_at: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct ProjectMember {
    pub principal_id: String,
    pub role: String,
}

#[derive(Clone)]
pub struct ProjectStore {
    db: Arc<Mutex<Connection>>,
}

impl ProjectStore {
    pub fn new(db: Arc<Mutex<Connection>>) -> Self {
        Self { db }
    }

    pub fn create(
        &self,
        channel: &str,
        name: &str,
        description: Option<&str>,
        created_by: &str,
    ) -> Result<Project> {
        if channel.eq_ignore_ascii_case(CHAT_ONLY_CHANNEL) {
            return Err(anyhow!(
                "{CHAT_ONLY_CHANNEL} is chat only and cannot be a project"
            ));
        }
        let name = name.trim();
        if name.is_empty() {
            return Err(anyhow!("project name is required"));
        }
        let now = unix_timestamp();
        let db = self.db.lock().expect("sqlite mutex poisoned");
        let inserted = db
            .execute(
                "INSERT OR IGNORE INTO projects (channel, name, description, created_by, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    channel,
                    name,
                    description.map(str::trim).filter(|d| !d.is_empty()),
                    created_by,
                    now
                ],
            )
            .context("insert project")?;
        if inserted == 0 {
            return Err(anyhow!("project {channel} already exists"));
        }
        Self::get_locked(&db, channel)?.ok_or_else(|| anyhow!("project {channel} vanished"))
    }

    pub fn get(&self, channel: &str) -> Result<Option<Project>> {
        let db = self.db.lock().expect("sqlite mutex poisoned");
        Self::get_locked(&db, channel)
    }

    pub fn list(&self) -> Result<Vec<Project>> {
        let db = self.db.lock().expect("sqlite mutex poisoned");
        let mut statement = db
            .prepare(
                "SELECT channel, name, description, created_by, created_at
                 FROM projects ORDER BY channel ASC",
            )
            .context("prepare project list")?;
        let projects = statement
            .query_map([], Self::from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(projects)
    }

    pub fn set_role(&self, channel: &str, principal_id: &str, role: &str) -> Result<()> {
        let db = self.db.lock().expect("sqlite mutex poisoned");
        db.execute(
            "INSERT INTO project_roles (channel, principal_id, role, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(channel, principal_id)
             DO UPDATE SET role = excluded.role, updated_at = excluded.updated_at",
            params![channel, principal_id, role, unix_timestamp()],
        )
        .context("set project role")?;
        Ok(())
    }

    pub fn role_of(&self, channel: &str, principal_id: &str) -> Result<Option<String>> {
        let db = self.db.lock().expect("sqlite mutex poisoned");
        db.query_row(
            "SELECT role FROM project_roles WHERE channel = ?1 AND principal_id = ?2",
            params![channel, principal_id],
            |row| row.get(0),
        )
        .optional()
        .context("load project role")
    }

    pub fn members(&self, channel: &str) -> Result<Vec<ProjectMember>> {
        let db = self.db.lock().expect("sqlite mutex poisoned");
        let mut statement = db
            .prepare(
                "SELECT principal_id, role FROM project_roles
                 WHERE channel = ?1 ORDER BY principal_id ASC",
            )
            .context("prepare project members")?;
        let members = statement
            .query_map(params![channel], |row| {
                Ok(ProjectMember {
                    principal_id: row.get(0)?,
                    role: row.get(1)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(members)
    }

    /// Projects where `principal_id` holds any role.
    pub fn projects_of(&self, principal_id: &str) -> Result<Vec<(String, String)>> {
        let db = self.db.lock().expect("sqlite mutex poisoned");
        let mut statement = db
            .prepare(
                "SELECT channel, role FROM project_roles
                 WHERE principal_id = ?1 ORDER BY channel ASC",
            )
            .context("prepare projects of principal")?;
        let rows = statement
            .query_map(params![principal_id], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn get_locked(db: &Connection, channel: &str) -> Result<Option<Project>> {
        db.query_row(
            "SELECT channel, name, description, created_by, created_at
             FROM projects WHERE channel = ?1",
            params![channel],
            Self::from_row,
        )
        .optional()
        .context("load project")
    }

    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Project> {
        Ok(Project {
            channel: row.get(0)?,
            name: row.get(1)?,
            description: row.get(2)?,
            created_by: row.get(3)?,
            created_at: row.get(4)?,
        })
    }
}
