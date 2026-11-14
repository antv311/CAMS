//! Descriptive task attributes, independent of workflow and completion output.
use super::*;

pub type TaskMetadata = serde_json::Map<String, serde_json::Value>;

pub fn normalize_tags(tags: &[String]) -> Result<Vec<String>> {
    if tags.len() > 64 {
        return Err(anyhow!("a task accepts at most 64 tags"));
    }
    if tags
        .iter()
        .any(|tag| tag.trim().is_empty() || tag.len() > 128 || tag.chars().any(char::is_control))
    {
        return Err(anyhow!(
            "tags must contain 1..128 UTF-8 bytes, be nonblank and have no control characters"
        ));
    }
    let mut tags = tags.to_vec();
    tags.sort();
    tags.dedup();
    Ok(tags)
}

pub fn validate_metadata(metadata: &TaskMetadata) -> Result<()> {
    if serde_json::to_vec(metadata)?.len() > 65536 {
        return Err(anyhow!("task metadata exceeds 65536 bytes"));
    }
    Ok(())
}

impl TaskStore {
    /// Replace supplied fields atomically. An omitted field is left untouched.
    pub fn set_attributes(
        &self,
        task_id: &str,
        tags: Option<&[String]>,
        metadata: Option<&TaskMetadata>,
        actor_id: &str,
    ) -> Result<Task> {
        if tags.is_none() && metadata.is_none() {
            return Err(anyhow!("supply tags and/or metadata"));
        }
        let tags = tags.map(normalize_tags).transpose()?;
        if let Some(metadata) = metadata {
            validate_metadata(metadata)?;
        }
        let mut db = self.db.write()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = Self::get_locked(&tx, task_id)?;
        let tags = tags.as_ref().unwrap_or(&current.tags);
        let metadata = metadata.unwrap_or(&current.metadata);
        if tags == &current.tags && metadata == &current.metadata {
            return Ok(current);
        }
        let now = unix_timestamp();
        tx.execute(
            "UPDATE tasks SET tags_json = ?2, metadata_json = ?3, updated_at = ?4 WHERE id = ?1",
            params![
                task_id,
                serde_json::to_string(tags)?,
                serde_json::to_string(metadata)?,
                now
            ],
        )?;
        // Record which fields changed without copying arbitrary metadata into chat.
        let note = format!(
            "tags_changed={} metadata_changed={}",
            tags != &current.tags,
            metadata != &current.metadata
        );
        Self::log_event_locked(
            &tx,
            task_id,
            actor_id,
            "attributes",
            Some(&current.status),
            Some(&current.status),
            Some(&note),
            now,
        )?;
        let task = Self::get_locked(&tx, task_id)?;
        tx.commit()?;
        Ok(task)
    }
}
