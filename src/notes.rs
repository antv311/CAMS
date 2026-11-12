//! Account notes: an account's lasting memory, kept on the hub.
//!
//! An agent reads its notes when it wakes and updates them as it works, so a
//! fresh launch or a reset conversation still knows what matters.

use anyhow::{anyhow, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::sync::{Arc, Mutex};

use crate::history::unix_timestamp;

/// Notes are a working memory, not an archive.
pub const MAX_NOTES_CHARS: usize = 16_000;

#[derive(Clone, Debug, Serialize)]
pub struct AccountNotes {
    pub principal_id: String,
    pub body: String,
    pub updated_at: Option<i64>,
}

#[derive(Clone)]
pub struct NoteStore {
    db: Arc<Mutex<Connection>>,
}

impl NoteStore {
    pub fn new(db: Arc<Mutex<Connection>>) -> Self {
        Self { db }
    }

    pub fn get(&self, principal_id: &str) -> Result<AccountNotes> {
        let db = self.db.lock().expect("sqlite mutex poisoned");
        let row: Option<(String, i64)> = db
            .query_row(
                "SELECT body, updated_at FROM account_notes WHERE principal_id = ?1",
                params![principal_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .context("load account notes")?;
        Ok(match row {
            Some((body, updated_at)) => AccountNotes {
                principal_id: principal_id.to_string(),
                body,
                updated_at: Some(updated_at),
            },
            None => AccountNotes {
                principal_id: principal_id.to_string(),
                body: String::new(),
                updated_at: None,
            },
        })
    }

    /// Replace the notes, or add a line to the end when `append` is set.
    pub fn write(&self, principal_id: &str, text: &str, append: bool) -> Result<AccountNotes> {
        let body = if append {
            let current = self.get(principal_id)?.body;
            let addition = text.trim();
            if addition.is_empty() {
                return Err(anyhow!("nothing to add"));
            }
            if current.is_empty() {
                addition.to_string()
            } else {
                format!("{}\n{}", current.trim_end(), addition)
            }
        } else {
            text.trim().to_string()
        };
        if body.chars().count() > MAX_NOTES_CHARS {
            return Err(anyhow!(
                "notes would exceed {MAX_NOTES_CHARS} characters; rewrite them shorter with a full replace"
            ));
        }
        let now = unix_timestamp();
        let db = self.db.lock().expect("sqlite mutex poisoned");
        db.execute(
            "INSERT INTO account_notes (principal_id, body, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(principal_id)
             DO UPDATE SET body = excluded.body, updated_at = excluded.updated_at",
            params![principal_id, body, now],
        )
        .context("save account notes")?;
        Ok(AccountNotes {
            principal_id: principal_id.to_string(),
            body,
            updated_at: Some(now),
        })
    }
}
