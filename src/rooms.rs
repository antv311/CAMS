//! Sprint rooms: numbered rooms under a project (s1, s2, s3 ...).
//!
//! A room is opened for a sprint and never closed. Tasks carry the room
//! they belong to, so a commit leads to its task, the task to its sprint,
//! and the room's history explains why the work was done. That history,
//! stored on the hub, is the lasting record of the project.

use anyhow::{anyhow, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use crate::history::unix_timestamp;
use crate::store::SharedConnection;

#[derive(Clone, Debug, Serialize)]
pub struct Room {
    /// The room's own channel, such as `#quest-video-s2`.
    pub channel: String,
    /// The project channel it belongs to.
    pub project: String,
    /// Short name within the project: `s1`, `s2`, ...
    pub label: String,
    pub title: Option<String>,
    pub opened_by: String,
    pub opened_at: i64,
}

#[derive(Clone)]
pub struct RoomStore {
    db: SharedConnection,
}

const ROOM_COLUMNS: &str = "channel, project, label, title, opened_by, opened_at";

impl RoomStore {
    pub fn new(db: SharedConnection) -> Self {
        Self { db }
    }

    /// Open the next sprint room of `project`.
    pub fn open(&self, project: &str, title: Option<&str>, opened_by: &str) -> Result<Room> {
        let db = self.db.write()?;
        let existing: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM rooms WHERE project = ?1",
                params![project],
                |row| row.get(0),
            )
            .context("count rooms")?;
        let label = format!("s{}", existing + 1);
        let channel = format!("{project}-{label}");
        let taken: Option<String> = db
            .query_row(
                "SELECT channel FROM projects WHERE channel = ?1",
                params![channel],
                |row| row.get(0),
            )
            .optional()
            .context("check room name")?;
        if taken.is_some() {
            return Err(anyhow!("{channel} is already a project; rename that project first"));
        }
        db.execute(
            "INSERT INTO rooms (channel, project, label, title, opened_by, opened_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                channel,
                project,
                label,
                title.map(str::trim).filter(|t| !t.is_empty()),
                opened_by,
                unix_timestamp()
            ],
        )
        .context("open room")?;
        Self::get_locked(&db, &channel)?.ok_or_else(|| anyhow!("room {channel} vanished"))
    }

    pub fn get(&self, channel: &str) -> Result<Option<Room>> {
        let db = self.db.read()?;
        Self::get_locked(&db, channel)
    }

    /// Rooms of one project, or of every project, oldest first.
    pub fn list(&self, project: Option<&str>) -> Result<Vec<Room>> {
        let db = self.db.read()?;
        let mut statement = db
            .prepare(&format!(
                "SELECT {ROOM_COLUMNS} FROM rooms
                 WHERE (?1 IS NULL OR project = ?1)
                 ORDER BY project ASC, opened_at ASC, rowid ASC"
            ))
            .context("prepare room list")?;
        let rooms = statement
            .query_map(params![project], Self::from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rooms)
    }

    /// The most recently opened room: where new tasks go by default.
    pub fn current(&self, project: &str) -> Result<Option<Room>> {
        Ok(self.list(Some(project))?.pop())
    }

    fn get_locked(db: &Connection, channel: &str) -> Result<Option<Room>> {
        db.query_row(
            &format!("SELECT {ROOM_COLUMNS} FROM rooms WHERE channel = ?1"),
            params![channel],
            Self::from_row,
        )
        .optional()
        .context("load room")
    }

    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Room> {
        Ok(Room {
            channel: row.get(0)?,
            project: row.get(1)?,
            label: row.get(2)?,
            title: row.get(3)?,
            opened_by: row.get(4)?,
            opened_at: row.get(5)?,
        })
    }
}
