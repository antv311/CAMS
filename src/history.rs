use anyhow::{Context, Result};
use rusqlite::{params, params_from_iter, types::Value as SqlValue};
use serde::Serialize;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

use crate::store::SharedConnection;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistorySearchScope {
    All,
    Sprints,
    Dms,
    Channel,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParsedSearchQuery {
    pub terms: Vec<String>,
    pub from_filters: Vec<String>,
    pub in_filters: Vec<String>,
}

impl ParsedSearchQuery {
    pub fn parse(raw: &str) -> Self {
        let mut parsed = Self::default();
        for token in raw.split_whitespace() {
            let lowered = token.to_ascii_lowercase();
            if let Some(who) = lowered.strip_prefix("from:") {
                let who = who.trim_start_matches('@');
                if !who.is_empty() {
                    parsed.from_filters.push(who.to_string());
                }
            } else if let Some(room) = lowered.strip_prefix("in:") {
                let room = room.trim_start_matches('@');
                if !room.is_empty() {
                    parsed.in_filters.push(room.to_string());
                }
            } else if !lowered.is_empty() {
                parsed.terms.push(lowered);
            }
        }
        parsed
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty() && self.from_filters.is_empty() && self.in_filters.is_empty()
    }
}

fn escape_like_pattern(term: &str) -> String {
    let mut out = String::with_capacity(term.len() + 2);
    out.push('%');
    for ch in term.chars() {
        match ch {
            '\\' | '%' | '_' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out.push('%');
    out
}

#[derive(Clone, Debug, Serialize)]
pub struct StoredMessage {
    pub seq: i64,
    pub msg_id: String,
    pub channel: String,
    pub sender_id: String,
    pub body: String,
    pub created_at: i64,
}

#[derive(Clone)]
pub struct HistoryStore {
    db: SharedConnection,
}

impl HistoryStore {
    pub fn new(db: SharedConnection) -> Self {
        Self { db }
    }

    pub fn append(&self, channel: &str, sender_id: &str, body: &str) -> Result<StoredMessage> {
        let msg_id = format!("msg_{}", Uuid::new_v4().simple());
        let created_at = unix_timestamp();
        let db = self.db.write()?;

        db.execute(
            "INSERT INTO messages (msg_id, channel, sender_id, body, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![msg_id, channel, sender_id, body, created_at],
        )
        .context("insert message")?;

        let seq = db.last_insert_rowid();

        Ok(StoredMessage {
            seq,
            msg_id,
            channel: channel.to_string(),
            sender_id: sender_id.to_string(),
            body: body.to_string(),
            created_at,
        })
    }

    pub fn after(&self, channel: &str, after_seq: i64, limit: i64) -> Result<Vec<StoredMessage>> {
        let capped_limit = limit.clamp(1, 500);
        let db = self.db.read()?;
        let mut statement = db
            .prepare(
                "SELECT seq, msg_id, channel, sender_id, body, created_at
                 FROM messages
                 WHERE channel = ?1 AND seq > ?2
                 ORDER BY seq ASC
                 LIMIT ?3",
            )
            .context("prepare history query")?;

        let messages = statement
            .query_map(params![channel, after_seq, capped_limit], |row| {
                Ok(StoredMessage {
                    seq: row.get(0)?,
                    msg_id: row.get(1)?,
                    channel: row.get(2)?,
                    sender_id: row.get(3)?,
                    body: row.get(4)?,
                    created_at: row.get(5)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(messages)
    }

    pub fn tail(&self, channel: &str, after_seq: i64, limit: i64) -> Result<Vec<StoredMessage>> {
        if after_seq > 0 {
            return self.after(channel, after_seq, limit);
        }
        let capped_limit = limit.clamp(1, 500);
        let db = self.db.read()?;
        let mut statement = db
            .prepare(
                "SELECT seq, msg_id, channel, sender_id, body, created_at
                 FROM (
                   SELECT seq, msg_id, channel, sender_id, body, created_at
                   FROM messages
                   WHERE channel = ?1
                   ORDER BY seq DESC
                   LIMIT ?2
                 )
                 ORDER BY seq ASC",
            )
            .context("prepare history tail query")?;

        let messages = statement
            .query_map(params![channel, capped_limit], |row| {
                Ok(StoredMessage {
                    seq: row.get(0)?,
                    msg_id: row.get(1)?,
                    channel: row.get(2)?,
                    sender_id: row.get(3)?,
                    body: row.get(4)?,
                    created_at: row.get(5)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(messages)
    }

    pub fn list_dm_messages(
        &self,
        principal_id: &str,
        peer_id: Option<&str>,
        after_seq: i64,
        limit: i64,
    ) -> Result<Vec<StoredMessage>> {
        if let Some(peer) = peer_id {
            let dm_chan = canonical_dm_channel(principal_id, peer);
            return self.tail(&dm_chan, after_seq, limit);
        }

        let capped_limit = limit.clamp(1, 500);
        let me = principal_id.trim().to_ascii_lowercase();
        let left_pattern = format!("@dm:{me}:%");
        let right_pattern = format!("@dm:%:{me}");
        let db = self.db.read()?;

        let mut statement = db
            .prepare(
                "SELECT seq, msg_id, channel, sender_id, body, created_at
                 FROM (
                   SELECT seq, msg_id, channel, sender_id, body, created_at
                   FROM messages
                   WHERE (channel LIKE ?1 OR channel LIKE ?2)
                     AND seq > ?3
                   ORDER BY seq DESC
                   LIMIT ?4
                 )
                 ORDER BY seq ASC",
            )
            .context("prepare dm list query")?;

        let messages = statement
            .query_map(
                params![left_pattern, right_pattern, after_seq.max(0), capped_limit],
                |row| {
                    Ok(StoredMessage {
                        seq: row.get(0)?,
                        msg_id: row.get(1)?,
                        channel: row.get(2)?,
                        sender_id: row.get(3)?,
                        body: row.get(4)?,
                        created_at: row.get(5)?,
                    })
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(messages)
    }

    pub fn recent_channels(&self) -> Result<Vec<(String, i64, Option<i64>)>> {
        let db = self.db.read()?;
        let mut stmt = db
            .prepare(
                "SELECT channel,
                        COUNT(*) AS msg_count,
                        MAX(created_at) AS last_at
                 FROM (
                   SELECT channel, created_at FROM messages WHERE channel NOT LIKE '@dm:%'
                   UNION ALL
                   SELECT channel, NULL AS created_at FROM channel_memberships
                 )
                 GROUP BY channel
                 ORDER BY COALESCE(MAX(created_at), 0) DESC, channel ASC",
            )
            .context("prepare recent channels")?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Search conversation history visible to `principal_id` (public channels and
    /// direct messages involving `principal_id`), ordered newest first.
    pub fn search(
        &self,
        principal_id: &str,
        query: &str,
        scope: HistorySearchScope,
        channel: Option<&str>,
        limit: i64,
    ) -> Result<Vec<StoredMessage>> {
        let parsed = ParsedSearchQuery::parse(query);
        if parsed.is_empty() {
            return Ok(Vec::new());
        }

        let capped_limit = limit.clamp(1, 200);
        let me = principal_id.trim().to_ascii_lowercase();
        let left_pattern = format!("@dm:{me}:%");
        let right_pattern = format!("@dm:%:{me}");

        let mut clauses: Vec<String> = Vec::new();
        let mut sql_params: Vec<SqlValue> = Vec::new();

        // Enforce DM privacy regardless of requested scope.
        clauses.push("(channel NOT LIKE '@dm:%' OR channel LIKE ? OR channel LIKE ?)".to_string());
        sql_params.push(SqlValue::Text(left_pattern.clone()));
        sql_params.push(SqlValue::Text(right_pattern.clone()));

        match scope {
            HistorySearchScope::All => {}
            HistorySearchScope::Sprints => {
                clauses.push("channel IN (SELECT channel FROM rooms)".to_string());
            }
            HistorySearchScope::Dms => {
                clauses.push("(channel LIKE ? OR channel LIKE ?)".to_string());
                sql_params.push(SqlValue::Text(left_pattern));
                sql_params.push(SqlValue::Text(right_pattern));
            }
            HistorySearchScope::Channel => {
                if let Some(target) = channel {
                    clauses.push("channel = ?".to_string());
                    sql_params.push(SqlValue::Text(target.to_string()));
                } else {
                    return Ok(Vec::new());
                }
            }
        }

        for who in &parsed.from_filters {
            clauses.push("LOWER(sender_id) LIKE ? ESCAPE '\\'".to_string());
            sql_params.push(SqlValue::Text(escape_like_pattern(who)));
        }
        for room in &parsed.in_filters {
            clauses.push("LOWER(channel) LIKE ? ESCAPE '\\'".to_string());
            sql_params.push(SqlValue::Text(escape_like_pattern(room)));
        }
        for term in &parsed.terms {
            clauses.push(
                "(LOWER(body) LIKE ? ESCAPE '\\' OR LOWER(sender_id) LIKE ? ESCAPE '\\')"
                    .to_string(),
            );
            let pattern = escape_like_pattern(term);
            sql_params.push(SqlValue::Text(pattern.clone()));
            sql_params.push(SqlValue::Text(pattern));
        }

        sql_params.push(SqlValue::Integer(capped_limit));
        let sql = format!(
            "SELECT seq, msg_id, channel, sender_id, body, created_at
             FROM messages
             WHERE {}
             ORDER BY seq DESC
             LIMIT ?",
            clauses.join(" AND ")
        );

        let db = self.db.read()?;
        let mut stmt = db.prepare(&sql).context("prepare history search query")?;
        let messages = stmt
            .query_map(params_from_iter(sql_params), |row| {
                Ok(StoredMessage {
                    seq: row.get(0)?,
                    msg_id: row.get(1)?,
                    channel: row.get(2)?,
                    sender_id: row.get(3)?,
                    body: row.get(4)?,
                    created_at: row.get(5)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(messages)
    }
}

pub fn canonical_dm_channel(a: &str, b: &str) -> String {
    let left = a.trim().to_ascii_lowercase();
    let right = b.trim().to_ascii_lowercase();
    if left <= right {
        format!("@dm:{left}:{right}")
    } else {
        format!("@dm:{right}:{left}")
    }
}

pub fn unix_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before Unix epoch")
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rooms::RoomStore;
    use crate::store;
    use std::path::PathBuf;

    struct TempDb(PathBuf);

    impl TempDb {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("aircd-history-test-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn search_filters_by_scope_privacy_and_qualifiers() -> Result<()> {
        let tmp = TempDb::new();
        let db = store::open(tmp.0.join("hub.sqlite3"))?;
        let history = HistoryStore::new(db.clone());
        let rooms = RoomStore::new(db);

        rooms.open("#cams", Some("Sprint 1"), "alice")?;
        history.append("#general", "alice", "Hello hub world")?;
        history.append("#cams-s1", "bob", "Merged 100% of search_modal fix")?;
        history.append("#cams-s1", "alice", "Another sprint update")?;
        history.append(&canonical_dm_channel("alice", "bob"), "bob", "Private DM fix note")?;
        history.append(&canonical_dm_channel("bob", "carol"), "carol", "Secret DM fix note")?;

        // Empty query returns nothing.
        assert!(history
            .search("alice", "   ", HistorySearchScope::All, None, 50)?
            .is_empty());

        // All scope finds channel + sprint + alice's DM, never bob<->carol DM.
        let all = history.search("alice", "fix", HistorySearchScope::All, None, 50)?;
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].channel, canonical_dm_channel("alice", "bob"));
        assert_eq!(all[1].channel, "#cams-s1");

        // Sprints scope restricts to rooms table.
        let sprints = history.search("alice", "fix", HistorySearchScope::Sprints, None, 50)?;
        assert_eq!(sprints.len(), 1);
        assert_eq!(sprints[0].channel, "#cams-s1");

        // DMs scope restricts to alice's DMs.
        let dms = history.search("alice", "fix", HistorySearchScope::Dms, None, 50)?;
        assert_eq!(dms.len(), 1);
        assert_eq!(dms[0].sender_id, "bob");

        // LIKE wildcards (% and _) are escaped properly.
        let literal = history.search("alice", "100% search_modal", HistorySearchScope::All, None, 50)?;
        assert_eq!(literal.len(), 1);
        assert!(literal[0].body.contains("100% of search_modal"));

        // Qualifiers from: and in: narrow down results.
        let qual = history.search(
            "alice",
            "from:alice in:#cams-s1 update",
            HistorySearchScope::All,
            None,
            50,
        )?;
        assert_eq!(qual.len(), 1);
        assert_eq!(qual[0].body, "Another sprint update");

        // Channel scope cannot leak another user's DM even if raw @dm: channel is passed.
        let forbidden = history.search(
            "alice",
            "Secret",
            HistorySearchScope::Channel,
            Some(&canonical_dm_channel("bob", "carol")),
            50,
        )?;
        assert!(forbidden.is_empty());

        Ok(())
    }
}
