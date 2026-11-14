use anyhow::{anyhow, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

pub use crate::db::{SharedConnection, SqlitePoolConfig, WalCheckpointMode};
use crate::history::unix_timestamp;
use crate::tokens;

#[path = "group_memory.rs"]
mod group_memory;
pub use group_memory::GroupMemoryError;

#[path = "group_activity.rs"]
mod group_activity;

/// A reusable seat definition. Counts describe workers; the lead has one seat.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupMember {
    pub template: String,
    pub model: String,
    #[serde(default = "one_seat")]
    pub count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nick: Option<String>,
}

fn one_seat() -> u32 {
    1
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupInput {
    pub name: String,
    pub description: Option<String>,
    pub lead: GroupMember,
    #[serde(default)]
    pub workers: Vec<GroupMember>,
    pub lead_prompt: Option<String>,
    pub worker_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_policy: Option<GroupReviewPolicy>,
    /// Opt in to automatic task takeover; absent/null disables it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inactivity_timeout_secs: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GroupReviewPolicy {
    pub peer_reviewers: Vec<String>,
    /// Omitted means every configured peer other than the task assignee.
    pub quorum: Option<u32>,
}

fn validate_review_policy(input: &mut GroupInput) -> Result<()> {
    let Some(policy) = &mut input.review_policy else {
        return Ok(());
    };
    let roster: std::collections::HashSet<String> = std::iter::once(&input.lead)
        .chain(&input.workers)
        .filter_map(|member| member.nick.as_deref())
        .map(|nick| nick.trim_start_matches('@').to_ascii_lowercase())
        .collect();
    let mut seen = std::collections::HashSet::new();
    for nick in &mut policy.peer_reviewers {
        *nick = nick.trim().trim_start_matches('@').to_ascii_lowercase();
        if !roster.contains(nick) || !seen.insert(nick.clone()) {
            return Err(anyhow!(
                "peer reviewers must be distinct named members of the group roster"
            ));
        }
    }
    if seen.is_empty()
        || policy
            .quorum
            .is_some_and(|q| q == 0 || q as usize > seen.len())
    {
        return Err(anyhow!(
            "review quorum must be between one and the number of configured peers"
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Group {
    #[serde(flatten)]
    pub definition: GroupInput,
    pub projects: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

pub struct NamedGroupRole {
    pub group: Group,
    pub is_lead: bool,
}

/// Atomic changes to an existing group's seats; other definition fields are preserved.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct GroupMembershipUpdate {
    pub lead: Option<GroupMember>,
    #[serde(default)]
    pub add_workers: Vec<GroupMember>,
    #[serde(default)]
    pub remove_workers: Vec<GroupMember>,
}

#[derive(Debug)]
pub enum GroupStoreError {
    GroupNotFound(String),
    ProjectNotFound(String),
}

impl std::fmt::Display for GroupStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::GroupNotFound(name) => write!(f, "no group named '{name}'"),
            Self::ProjectNotFound(channel) => write!(f, "no project named '{channel}'"),
        }
    }
}

impl std::error::Error for GroupStoreError {}

/// Definition and attachment persistence only; does not create or launch accounts.
#[derive(Clone)]
pub struct GroupStore {
    db: SharedConnection,
}

impl GroupStore {
    pub fn new(db: SharedConnection) -> Self {
        Self { db }
    }

    /// Full replacement by name, preserving original creation time and attachments.
    pub fn upsert(&self, mut input: GroupInput) -> Result<Group> {
        input.name = group_name(&input.name)?.to_string();
        validate_group_member(&mut input.lead)?;
        if input.lead.count != 1 {
            return Err(anyhow!("a group must have exactly one lead"));
        }
        if input.workers.is_empty() {
            return Err(anyhow!("a group needs at least one worker"));
        }
        for worker in &mut input.workers {
            validate_group_member(worker)?;
        }
        validate_review_policy(&mut input)?;
        if input
            .inactivity_timeout_secs
            .is_some_and(|seconds| seconds <= 0)
        {
            return Err(anyhow!(
                "inactivity_timeout_secs must be a positive integer or null"
            ));
        }
        for text in [&input.description, &input.lead_prompt, &input.worker_prompt]
            .into_iter()
            .flatten()
        {
            if text.contains('\0') {
                return Err(anyhow!(
                    "group description and prompts must not contain NUL"
                ));
            }
        }
        let db = self.db.write()?;
        db.execute(
            "INSERT INTO groups
               (name, description, lead_json, workers_json, lead_prompt, worker_prompt,
                created_at, updated_at, review_policy_json, inactivity_timeout_secs)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8, ?9)
             ON CONFLICT(name) DO UPDATE SET
               description = excluded.description, lead_json = excluded.lead_json,
               workers_json = excluded.workers_json, lead_prompt = excluded.lead_prompt,
               worker_prompt = excluded.worker_prompt, updated_at = excluded.updated_at,
               review_policy_json = excluded.review_policy_json,
               inactivity_timeout_secs = excluded.inactivity_timeout_secs",
            params![
                input.name,
                input.description,
                serde_json::to_string(&input.lead)?,
                serde_json::to_string(&input.workers)?,
                input.lead_prompt,
                input.worker_prompt,
                unix_timestamp(),
                input
                    .review_policy
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()?,
                input.inactivity_timeout_secs,
            ],
        )
        .context("save group")?;
        Self::get_locked(&db, &input.name)?.ok_or_else(|| anyhow!("group {} vanished", input.name))
    }

    pub fn get(&self, name: &str) -> Result<Option<Group>> {
        let name = group_name(name)?;
        let db = self.db.read()?;
        Self::get_locked(&db, name)
    }

    /// Only explicit nick bindings identify accounts; model/count seats do not.
    pub fn named_role(&self, project: &str, nick: &str) -> Result<Option<NamedGroupRole>> {
        let mut selected = None;
        for group in self.list()? {
            if !group.projects.iter().any(|channel| channel == project) {
                continue;
            }
            let matches = |member: &GroupMember| {
                member.nick.as_deref().is_some_and(|name| {
                    name.trim_start_matches('@')
                        .eq_ignore_ascii_case(nick.trim_start_matches('@'))
                })
            };
            let is_lead = matches(&group.definition.lead);
            let is_worker = group.definition.workers.iter().any(matches);
            if !is_lead && !is_worker {
                continue;
            }
            if selected.is_some() || (is_lead && is_worker) {
                return Err(anyhow!(
                    "ambiguous group role for @{nick} in {project}; bind this nick to one group role per project"
                ));
            }
            selected = Some(NamedGroupRole { group, is_lead });
        }
        Ok(selected)
    }

    pub fn update_membership(
        &self,
        name: &str,
        mut update: GroupMembershipUpdate,
    ) -> Result<Group> {
        let name = group_name(name)?;
        if update.lead.is_none()
            && update.add_workers.is_empty()
            && update.remove_workers.is_empty()
        {
            return Err(anyhow!(
                "group update needs a lead, add_workers, or remove_workers"
            ));
        }
        if let Some(lead) = &mut update.lead {
            validate_group_member(lead)?;
            if lead.count != 1 {
                return Err(anyhow!("a group must have exactly one lead"));
            }
        }
        for member in update
            .add_workers
            .iter_mut()
            .chain(&mut update.remove_workers)
        {
            validate_group_member(member)?;
        }
        let mut db = self.db.write()?;
        // Reserve the write before reading, including across separate DB connections.
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut group = Self::get_locked(&tx, name)?
            .ok_or_else(|| GroupStoreError::GroupNotFound(name.to_string()))?;
        let before = group.definition.clone();
        if let Some(lead) = update.lead {
            group.definition.lead = lead;
        }
        let workers = &mut group.definition.workers;
        // Remove first so a single request can replace the last worker kind.
        for removal in update.remove_workers {
            let available: u64 = workers
                .iter()
                .filter(|w| same_seat(w, &removal))
                .map(|w| u64::from(w.count))
                .sum();
            if available < u64::from(removal.count) {
                return Err(anyhow!(
                    "cannot remove {} worker seat(s) for {}:{}: only {available} matching seats",
                    removal.count,
                    removal.template,
                    removal.model
                ));
            }
            let mut remaining = removal.count;
            for worker in workers.iter_mut().filter(|w| same_seat(w, &removal)) {
                let removed = worker.count.min(remaining);
                worker.count -= removed;
                remaining -= removed;
            }
            workers.retain(|w| w.count > 0);
        }
        for addition in update.add_workers {
            if let Some(worker) = workers.iter_mut().find(|w| same_seat(w, &addition)) {
                worker.count = worker
                    .count
                    .checked_add(addition.count)
                    .ok_or_else(|| anyhow!("group worker count exceeds {}", u32::MAX))?;
            } else {
                workers.push(addition);
            }
        }
        if workers.is_empty() {
            return Err(anyhow!("a group needs at least one worker"));
        }
        validate_review_policy(&mut group.definition)?;
        if group.definition != before {
            tx.execute(
                "UPDATE groups SET lead_json = ?2, workers_json = ?3, updated_at = ?4 WHERE name = ?1",
                params![name, serde_json::to_string(&group.definition.lead)?,
                    serde_json::to_string(&group.definition.workers)?, unix_timestamp()],
            )?;
        }
        let saved = Self::get_locked(&tx, name)?.ok_or_else(|| anyhow!("group {name} vanished"))?;
        tx.commit()?;
        Ok(saved)
    }

    pub fn list(&self) -> Result<Vec<Group>> {
        let db = self.db.read()?;
        let mut statement = db.prepare("SELECT name FROM groups ORDER BY name ASC")?;
        let names = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        names
            .into_iter()
            .map(|name| {
                Self::get_locked(&db, &name)?.ok_or_else(|| anyhow!("group {name} vanished"))
            })
            .collect()
    }

    /// Attach only to real projects. Repeated attachment leaves timestamps unchanged.
    pub fn attach(&self, name: &str, channel: &str) -> Result<Group> {
        let name = group_name(name)?;
        let channel = channel.trim();
        if channel.len() < 2
            || !channel.starts_with('#')
            || channel.chars().any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(anyhow!(
                "project channel must start with '#' and contain no whitespace"
            ));
        }
        let db = self.db.write()?;
        let group = Self::get_locked(&db, name)?
            .ok_or_else(|| GroupStoreError::GroupNotFound(name.to_string()))?;
        let exists: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM projects WHERE channel = ?1)",
            [channel],
            |row| row.get(0),
        )?;
        if !exists {
            return Err(GroupStoreError::ProjectNotFound(channel.to_string()).into());
        }
        let tx = db.unchecked_transaction()?;
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO group_projects (group_name, channel) VALUES (?1, ?2)",
            params![name, channel],
        )?;
        if inserted == 0 {
            return Ok(group);
        }
        tx.execute(
            "UPDATE groups SET updated_at = ?2 WHERE name = ?1",
            params![name, unix_timestamp()],
        )?;
        tx.commit()?;
        Self::get_locked(&db, name)?.ok_or_else(|| anyhow!("group {name} vanished"))
    }

    pub fn delete(&self, name: &str) -> Result<bool> {
        let name = group_name(name)?;
        let db = self.db.write()?;
        let tx = db.unchecked_transaction()?;
        tx.execute("DELETE FROM group_memory WHERE group_name = ?1", [name])?;
        tx.execute(
            "DELETE FROM group_reassignments WHERE group_name = ?1",
            [name],
        )?;
        tx.execute("DELETE FROM group_projects WHERE group_name = ?1", [name])?;
        let changed = tx.execute("DELETE FROM groups WHERE name = ?1", [name])?;
        tx.commit()?;
        Ok(changed > 0)
    }

    fn get_locked(db: &Connection, name: &str) -> Result<Option<Group>> {
        let group = db
            .query_row(
                "SELECT name, description, lead_json, workers_json, lead_prompt,
                        worker_prompt, created_at, updated_at, review_policy_json,
                        inactivity_timeout_secs FROM groups WHERE name = ?1",
                [name],
                |row| {
                    let lead: String = row.get(2)?;
                    let workers: String = row.get(3)?;
                    let decode_error = |index, error: serde_json::Error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            index,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    };
                    Ok(Group {
                        definition: GroupInput {
                            name: row.get(0)?,
                            description: row.get(1)?,
                            lead: serde_json::from_str(&lead).map_err(|e| decode_error(2, e))?,
                            workers: serde_json::from_str(&workers)
                                .map_err(|e| decode_error(3, e))?,
                            lead_prompt: row.get(4)?,
                            worker_prompt: row.get(5)?,
                            review_policy: row
                                .get::<_, Option<String>>(8)?
                                .map(|raw| {
                                    serde_json::from_str(&raw).map_err(|e| decode_error(8, e))
                                })
                                .transpose()?,
                            inactivity_timeout_secs: row.get(9)?,
                        },
                        projects: Vec::new(),
                        created_at: row.get(6)?,
                        updated_at: row.get(7)?,
                    })
                },
            )
            .optional()
            .context("load group")?;
        let Some(mut group) = group else {
            return Ok(None);
        };
        let mut statement = db.prepare(
            "SELECT channel FROM group_projects WHERE group_name = ?1 ORDER BY channel ASC",
        )?;
        group.projects = statement
            .query_map([name], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Some(group))
    }
}

fn same_seat(left: &GroupMember, right: &GroupMember) -> bool {
    left.template == right.template && left.model == right.model && left.nick == right.nick
}

fn group_name(name: &str) -> Result<&str> {
    let name = name.trim();
    if name.is_empty()
        || matches!(name, "." | "..")
        || name.chars().count() > 100
        || name
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '\\'))
    {
        return Err(anyhow!(
            "group name must be one nonempty path segment of at most 100 characters"
        ));
    }
    Ok(name)
}

fn validate_group_member(member: &mut GroupMember) -> Result<()> {
    member.template = client_template_id(&member.template)?.to_string();
    member.model = member.model.trim().to_string();
    if !valid_model(&member.model) {
        return Err(anyhow!(
            "group member model must be one word of at most 100 characters without braces"
        ));
    }
    if member.count == 0 {
        return Err(anyhow!("group member count must be at least 1"));
    }
    if let Some(nick) = &member.nick {
        if nick.trim().is_empty() || nick.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(anyhow!("group member nick must be one nonempty word"));
        }
    }
    Ok(())
}

/// An owner-defined client launcher. Placeholders such as `{model}` and
/// `{prompt}` are stored verbatim; the chosen model belongs to the worker,
/// not to the template. Executable availability is runtime state and is not
/// stored here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientTemplate {
    pub id: String,
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    /// As with `WakeConfig`, `None` removes a variable from the child environment.
    pub env: BTreeMap<String, Option<String>>,
    pub prompt: Option<String>,
    pub session_mode: String,
    pub resume_args: Vec<String>,
    pub session_marker: Option<String>,
    pub session_scope: String,
    /// Models offered when an account is created from this template.
    pub models: Vec<String>,
    /// A command, program first, that prints the models: one per line with
    /// the id first (`["agy", "models"]`), or a model API's JSON with a `data`
    /// list of `id`s (a `curl` call). Empty when the list is kept by hand.
    pub models_command: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Full replacement input for a template. Missing session settings default
/// to a fresh conversation with account scope, matching the launcher.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ClientTemplateInput {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub env: BTreeMap<String, Option<String>>,
    pub prompt: Option<String>,
    pub session_mode: Option<String>,
    pub resume_args: Vec<String>,
    pub session_marker: Option<String>,
    pub session_scope: Option<String>,
    pub models: Vec<String>,
    pub models_command: Vec<String>,
}

/// Persistence only: callers enforce owner/PM permissions and provision accounts.
#[derive(Clone)]
pub struct ClientTemplateStore {
    db: SharedConnection,
}

const CLIENT_TEMPLATE_SELECT: &str =
    "SELECT id, name, command, args_json, cwd, env_json, prompt, session_mode,
            resume_args_json, session_marker, session_scope, created_at, updated_at,
            models_json, models_command_json
     FROM client_templates";

impl ClientTemplateStore {
    pub fn new(db: SharedConnection) -> Self {
        Self { db }
    }

    /// IDs are case-sensitive ASCII letters, digits, `-` or `_`, with outer
    /// whitespace trimmed. Create or replace the mutable fields of a template. Its original
    /// creation time is retained. No executable is invoked or model restricted.
    pub fn upsert(&self, id: &str, input: ClientTemplateInput) -> Result<ClientTemplate> {
        let id = client_template_id(id)?;
        let name = input.name.trim();
        let command = input.command.trim();
        if name.is_empty() || name.contains('\0') {
            return Err(anyhow!(
                "client template name is required and must not contain NUL"
            ));
        }
        if command.is_empty() || command.contains('\0') {
            return Err(anyhow!(
                "client template command is required and must not contain NUL"
            ));
        }
        let session_mode = template_setting(
            input.session_mode.as_deref(),
            &["fresh", "resume", "pm"],
            "session_mode",
        )?;
        let session_scope = template_setting(
            input.session_scope.as_deref(),
            &["account", "channel"],
            "session_scope",
        )?;
        let models = template_models(&input.models)?;
        if input
            .models_command
            .first()
            .is_some_and(|program| program.trim().is_empty() || program.contains('\0'))
        {
            return Err(anyhow!("models_command must start with the program to run"));
        }
        let db = self.db.write()?;
        db.execute(
            "INSERT INTO client_templates
               (id, name, command, args_json, cwd, env_json, prompt, session_mode,
                resume_args_json, session_marker, session_scope, created_at, updated_at,
                models_json, models_command_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?12, ?13, ?14)
             ON CONFLICT(id) DO UPDATE SET
               name = excluded.name, command = excluded.command,
               args_json = excluded.args_json, cwd = excluded.cwd,
               env_json = excluded.env_json, prompt = excluded.prompt,
               session_mode = excluded.session_mode,
               resume_args_json = excluded.resume_args_json,
               session_marker = excluded.session_marker,
               session_scope = excluded.session_scope, updated_at = excluded.updated_at,
               models_json = excluded.models_json,
               models_command_json = excluded.models_command_json",
            params![
                id,
                name,
                command,
                serde_json::to_string(&input.args)?,
                input
                    .cwd
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
                serde_json::to_string(&input.env)?,
                input.prompt.filter(|s| !s.trim().is_empty()),
                session_mode,
                serde_json::to_string(&input.resume_args)?,
                input.session_marker.filter(|s| !s.trim().is_empty()),
                session_scope,
                unix_timestamp(),
                serde_json::to_string(&models)?,
                serde_json::to_string(&input.models_command)?,
            ],
        )
        .context("save client template")?;
        Self::get_locked(&db, id)?.ok_or_else(|| anyhow!("client template {id} vanished"))
    }

    pub fn get(&self, id: &str) -> Result<Option<ClientTemplate>> {
        let id = client_template_id(id)?;
        let db = self.db.read()?;
        Self::get_locked(&db, id)
    }

    /// Stable directory order, independent of insertion or update order.
    pub fn list(&self) -> Result<Vec<ClientTemplate>> {
        let db = self.db.read()?;
        let mut statement = db
            .prepare(&format!("{CLIENT_TEMPLATE_SELECT} ORDER BY id ASC"))
            .context("prepare client template list")?;
        let rows = statement
            .query_map([], Self::from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("read client templates")?;
        Ok(rows)
    }

    /// Returns false when no template exists. Existing workers are untouched.
    pub fn delete(&self, id: &str) -> Result<bool> {
        let id = client_template_id(id)?;
        let db = self.db.write()?;
        let changed = db
            .execute("DELETE FROM client_templates WHERE id = ?1", params![id])
            .context("delete client template")?;
        Ok(changed > 0)
    }

    fn get_locked(db: &Connection, id: &str) -> Result<Option<ClientTemplate>> {
        db.query_row(
            &format!("{CLIENT_TEMPLATE_SELECT} WHERE id = ?1"),
            params![id],
            Self::from_row,
        )
        .optional()
        .context("load client template")
    }

    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ClientTemplate> {
        fn json<T: serde::de::DeserializeOwned>(
            row: &rusqlite::Row<'_>,
            index: usize,
        ) -> rusqlite::Result<T> {
            let value: String = row.get(index)?;
            serde_json::from_str(&value).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    index,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })
        }

        Ok(ClientTemplate {
            id: row.get(0)?,
            name: row.get(1)?,
            command: row.get(2)?,
            args: json(row, 3)?,
            cwd: row.get(4)?,
            env: json(row, 5)?,
            prompt: row.get(6)?,
            session_mode: row.get(7)?,
            resume_args: json(row, 8)?,
            session_marker: row.get(9)?,
            session_scope: row.get(10)?,
            created_at: row.get(11)?,
            updated_at: row.get(12)?,
            models: json(row, 13)?,
            models_command: json(row, 14)?,
        })
    }
}

impl ClientTemplate {
    /// The launcher an account created from this template gets: every
    /// `{model}` becomes `model`; the launch-time placeholders such as
    /// `{prompt}` and `{nick}` stay for the launcher to fill in.
    pub fn launcher_for(&self, model: &str) -> crate::wake::WakeConfigInput {
        let fill = |text: &str| text.replace("{model}", model);
        crate::wake::WakeConfigInput {
            command: fill(&self.command),
            args: self.args.iter().map(|a| fill(a)).collect(),
            cwd: self.cwd.as_deref().map(fill),
            env: self
                .env
                .iter()
                .map(|(k, v)| (k.clone(), v.as_deref().map(fill)))
                .collect(),
            prompt: self.prompt.as_deref().map(fill),
            enabled: Some(true),
            session_mode: Some(self.session_mode.clone()),
            resume_args: self.resume_args.iter().map(|a| fill(a)).collect(),
            session_marker: self.session_marker.clone(),
            session_scope: Some(self.session_scope.clone()),
            ..Default::default()
        }
    }
}

/// A model name ends up in launch arguments and, by default, as an account
/// name, so it must be one plain token.
pub fn valid_model(model: &str) -> bool {
    !model.is_empty()
        && model.len() <= 100
        && !model
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '{' | '}'))
}

fn template_models(models: &[String]) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for model in models {
        let model = model.trim();
        if !valid_model(model) {
            return Err(anyhow!(
                "model names must be one word of at most 100 characters without braces: {model:?}"
            ));
        }
        if !out.iter().any(|m| m == model) {
            out.push(model.to_string());
        }
    }
    Ok(out)
}

fn client_template_id(id: &str) -> Result<&str> {
    let id = id.trim();
    if id.is_empty()
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return Err(anyhow!(
            "client template id must contain only ASCII letters, digits, '-' or '_'"
        ));
    }
    Ok(id)
}

fn template_setting(value: Option<&str>, allowed: &[&str], name: &str) -> Result<String> {
    let value = value.unwrap_or(allowed[0]).trim().to_ascii_lowercase();
    let value = if value.is_empty() { allowed[0] } else { &value };
    if !allowed.contains(&value) {
        return Err(anyhow!("{name} must be one of: {}", allowed.join(", ")));
    }
    Ok(value.to_string())
}

/// Label used to mark `principal_tokens` rows that were backfilled from the
/// legacy `principals.token` column. The uniqueness constraint below prevents
/// double backfills even if `init` runs repeatedly.
const MIGRATED_LEGACY_LABEL: &str = "migrated-legacy";

/// Label used for rows created alongside seeded demo principals.
const SEED_LABEL: &str = "seed";

/// Demo/test principals whose tokens are published in the README. They are
/// only usable on in-memory databases (tests) or when `AIRCD_SEED_DEMO=1`.
const DEMO_PRINCIPALS: &[(&str, &str, &str)] = &[
    ("human", "human-token", "demo"),
    ("agent-a", "agent-a-token", "demo"),
    ("agent-b", "agent-b-token", "demo"),
    ("agent-c", "agent-c-token", "demo"),
    ("agent-1", "test-token-1", "test"),
    ("agent-2", "test-token-2", "test"),
    ("agent-3", "test-token-3", "test"),
];

#[allow(dead_code)]
pub fn open(path: impl AsRef<Path>) -> Result<SharedConnection> {
    open_with_config(path, SqlitePoolConfig::default())
}

pub fn open_with_config(
    path: impl AsRef<Path>,
    config: SqlitePoolConfig,
) -> Result<SharedConnection> {
    let path_ref = path.as_ref();
    let seed_demo = path_ref == Path::new(":memory:") || demo_seed_requested();
    SharedConnection::open_pool(path_ref, config, |connection| {
        init_with(connection, seed_demo)
    })
}

fn demo_seed_requested() -> bool {
    matches!(
        std::env::var("AIRCD_SEED_DEMO").ok().as_deref(),
        Some("1") | Some("true") | Some("TRUE") | Some("yes")
    )
}

#[cfg(test)]
fn init(connection: &Connection) -> Result<()> {
    init_with(connection, true)
}

/// Revoke the published demo credentials on a real database. Account names
/// and history are kept; only tokens that are public knowledge stop working.
fn disable_published_demo_credentials(connection: &Connection) -> Result<()> {
    for (id, token, _team) in DEMO_PRINCIPALS {
        connection
            .execute(
                "UPDATE principal_tokens SET revoked_at = unixepoch()
                 WHERE principal_id = ?1 AND revoked_at IS NULL
                   AND (label = ?2 OR (label = ?3 AND EXISTS (
                         SELECT 1 FROM principals WHERE id = ?1 AND token = ?4)))",
                params![id, SEED_LABEL, MIGRATED_LEGACY_LABEL, token],
            )
            .context("revoke published demo token")?;
        connection
            .execute(
                "UPDATE principals
                 SET token = '__hashed__:disabled-' || lower(hex(randomblob(16)))
                 WHERE id = ?1 AND token = ?2",
                params![id, token],
            )
            .context("disable published demo credential")?;
    }
    Ok(())
}

fn init_with(connection: &Connection, seed_demo: bool) -> Result<()> {
    connection
        .execute_batch(
            "
            PRAGMA journal_mode = WAL;

            CREATE TABLE IF NOT EXISTS principals (
              id TEXT PRIMARY KEY,
              nick TEXT NOT NULL,
              token TEXT UNIQUE NOT NULL,
              team TEXT,
              last_seen_seq INTEGER NOT NULL DEFAULT 0,
              connected_at INTEGER,
              disconnected_at INTEGER,
              created_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS channel_memberships (
              principal_id TEXT NOT NULL,
              channel TEXT NOT NULL,
              joined_at INTEGER NOT NULL,
              last_seen_seq INTEGER NOT NULL DEFAULT 0,
              PRIMARY KEY (principal_id, channel)
            );

            CREATE TABLE IF NOT EXISTS messages (
              seq INTEGER PRIMARY KEY AUTOINCREMENT,
              msg_id TEXT UNIQUE NOT NULL,
              channel TEXT NOT NULL,
              sender_id TEXT NOT NULL,
              body TEXT NOT NULL,
              created_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS tasks (
              id TEXT PRIMARY KEY,
              channel TEXT NOT NULL,
              title TEXT NOT NULL,
              status TEXT NOT NULL,
              claimed_by TEXT,
              lease_expires_at INTEGER,
              created_at INTEGER NOT NULL,
              updated_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS principal_tokens (
              id INTEGER PRIMARY KEY AUTOINCREMENT,
              principal_id TEXT NOT NULL REFERENCES principals(id) ON DELETE CASCADE,
              token_id TEXT UNIQUE NOT NULL,
              token_hash TEXT NOT NULL,
              label TEXT,
              scopes TEXT NOT NULL DEFAULT '[\"admin\"]',
              ttl_seconds INTEGER,
              expires_at INTEGER,
              created_at INTEGER NOT NULL,
              revoked_at INTEGER
            );

            CREATE INDEX IF NOT EXISTS idx_principal_tokens_principal
              ON principal_tokens (principal_id);

            -- Guarantees at most one backfill row per principal, even if init
            -- runs repeatedly against an already-populated database.
            CREATE UNIQUE INDEX IF NOT EXISTS idx_principal_tokens_principal_label
              ON principal_tokens (principal_id, label);

            CREATE TABLE IF NOT EXISTS webhooks (
              principal_id TEXT PRIMARY KEY REFERENCES principals(id) ON DELETE CASCADE,
              url TEXT NOT NULL,
              secret TEXT NOT NULL,
              events TEXT NOT NULL,
              notify_when TEXT NOT NULL DEFAULT 'always',
              enabled INTEGER NOT NULL DEFAULT 1,
              last_delivery_at INTEGER,
              last_status INTEGER,
              last_error TEXT,
              created_at INTEGER NOT NULL,
              updated_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS webhook_deliveries (
              id TEXT PRIMARY KEY,
              principal_id TEXT NOT NULL REFERENCES principals(id) ON DELETE CASCADE,
              event_type TEXT NOT NULL,
              channel TEXT NOT NULL,
              payload_json TEXT NOT NULL,
              status TEXT NOT NULL,
              attempts INTEGER NOT NULL DEFAULT 0,
              next_attempt_at INTEGER NOT NULL,
              http_status INTEGER,
              last_error TEXT,
              created_at INTEGER NOT NULL,
              updated_at INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_webhook_deliveries_due
              ON webhook_deliveries (status, next_attempt_at);

            CREATE INDEX IF NOT EXISTS idx_webhook_deliveries_principal
              ON webhook_deliveries (principal_id, created_at DESC);

            CREATE TABLE IF NOT EXISTS reports (
              id TEXT PRIMARY KEY,
              author_id TEXT NOT NULL REFERENCES principals(id) ON DELETE CASCADE,
              channel TEXT NOT NULL,
              title TEXT NOT NULL,
              status TEXT NOT NULL,
              summary TEXT NOT NULL,
              body TEXT NOT NULL,
              task_id TEXT,
              git_repo TEXT,
              git_branch TEXT,
              git_commit TEXT,
              created_at INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_reports_channel_created
              ON reports (channel, created_at DESC);

            CREATE TABLE IF NOT EXISTS git_repos (
              name TEXT PRIMARY KEY,
              channel TEXT NOT NULL,
              local_path TEXT NOT NULL,
              remote_url TEXT,
              default_branch TEXT,
              webhook_secret TEXT,
              last_commit TEXT,
              last_branch TEXT,
              last_event_at INTEGER,
              created_at INTEGER NOT NULL,
              updated_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS resource_locks (
              resource TEXT PRIMARY KEY,
              owner_id TEXT NOT NULL REFERENCES principals(id) ON DELETE CASCADE,
              channel TEXT NOT NULL,
              note TEXT,
              expires_at INTEGER NOT NULL,
              updated_at INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_resource_locks_channel
              ON resource_locks (channel, expires_at);

            CREATE TABLE IF NOT EXISTS projects (
              channel TEXT PRIMARY KEY,
              name TEXT NOT NULL,
              description TEXT,
              created_by TEXT NOT NULL,
              created_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS project_roles (
              channel TEXT NOT NULL,
              principal_id TEXT NOT NULL,
              role TEXT NOT NULL,
              updated_at INTEGER NOT NULL,
              PRIMARY KEY (channel, principal_id)
            );

            CREATE TABLE IF NOT EXISTS task_events (
              id INTEGER PRIMARY KEY AUTOINCREMENT,
              task_id TEXT NOT NULL,
              actor_id TEXT NOT NULL,
              action TEXT NOT NULL,
              from_status TEXT,
              to_status TEXT,
              note TEXT,
              created_at INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_task_events_task
              ON task_events (task_id, id);

            CREATE TABLE IF NOT EXISTS wake_configs (
              principal_id TEXT PRIMARY KEY,
              command TEXT NOT NULL,
              args_json TEXT NOT NULL,
              cwd TEXT,
              env_json TEXT NOT NULL,
              prompt TEXT,
              enabled INTEGER NOT NULL DEFAULT 1,
              cooldown_seconds INTEGER NOT NULL,
              timeout_seconds INTEGER NOT NULL,
              max_runs_per_hour INTEGER NOT NULL,
              updated_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS wake_runs (
              id TEXT PRIMARY KEY,
              principal_id TEXT NOT NULL,
              reason TEXT NOT NULL,
              channel TEXT NOT NULL,
              task_id TEXT,
              status TEXT NOT NULL,
              exit_code INTEGER,
              started_at INTEGER NOT NULL,
              finished_at INTEGER,
              log_tail TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_wake_runs_principal
              ON wake_runs (principal_id, started_at);

            CREATE TABLE IF NOT EXISTS rooms (
              channel TEXT PRIMARY KEY,
              project TEXT NOT NULL,
              label TEXT NOT NULL,
              title TEXT,
              opened_by TEXT NOT NULL,
              opened_at INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_rooms_project ON rooms (project, opened_at);

            CREATE TABLE IF NOT EXISTS task_questions (
              id TEXT PRIMARY KEY,
              task_id TEXT NOT NULL,
              channel TEXT NOT NULL,
              asked_by TEXT NOT NULL,
              question TEXT NOT NULL,
              status TEXT NOT NULL,
              resume_status TEXT NOT NULL,
              answer TEXT,
              answered_by TEXT,
              escalation_note TEXT,
              created_at INTEGER NOT NULL,
              answered_at INTEGER
            );

            CREATE INDEX IF NOT EXISTS idx_task_questions_status
              ON task_questions (status, channel);

            CREATE TABLE IF NOT EXISTS account_notes (
              principal_id TEXT PRIMARY KEY,
              body TEXT NOT NULL,
              updated_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS wake_sessions (
              principal_id TEXT NOT NULL,
              scope_key TEXT NOT NULL,
              session_id TEXT NOT NULL,
              updated_at INTEGER NOT NULL,
              PRIMARY KEY (principal_id, scope_key)
            );
            ",
        )
        .context("initialize sqlite schema")?;

    migrate_principals(connection)?;
    migrate_principal_tokens(connection)?;
    migrate_channel_memberships(connection)?;
    migrate_tasks(connection)?;
    migrate_wake_configs(connection)?;
    migrate_projects(connection)?;
    migrate_client_templates(connection)?;
    migrate_groups(connection)?;
    crate::attachments::migrate(connection)?;
    crate::token_usage::migrate(connection)?;

    if seed_demo {
        for (id, token, team) in DEMO_PRINCIPALS {
            seed_principal(connection, id, id, token, team)?;
        }
    } else {
        disable_published_demo_credentials(connection)?;
    }

    backfill_principal_tokens(connection)?;

    Ok(())
}

/// Additive and idempotent for databases created before client templates.
fn migrate_client_templates(connection: &Connection) -> Result<()> {
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS client_templates (
           id TEXT PRIMARY KEY NOT NULL,
           name TEXT NOT NULL,
           command TEXT NOT NULL,
           args_json TEXT NOT NULL DEFAULT '[]',
           cwd TEXT,
           env_json TEXT NOT NULL DEFAULT '{}',
           prompt TEXT,
           session_mode TEXT NOT NULL DEFAULT 'fresh'
             CHECK (session_mode IN ('fresh', 'resume', 'pm')),
           resume_args_json TEXT NOT NULL DEFAULT '[]',
           session_marker TEXT,
           session_scope TEXT NOT NULL DEFAULT 'account'
             CHECK (session_scope IN ('account', 'channel')),
           created_at INTEGER NOT NULL,
           updated_at INTEGER NOT NULL
         );",
        )
        .context("migrate client templates")?;
    for column in ["models_json", "models_command_json"] {
        let exists: i64 = connection.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('client_templates') WHERE name = ?1",
            params![column],
            |row| row.get(0),
        )?;
        if exists == 0 {
            connection.execute(
                &format!("ALTER TABLE client_templates ADD COLUMN {column} TEXT NOT NULL DEFAULT '[]'"),
                [],
            )?;
        }
    }
    Ok(())
}

/// Additive and safe to rerun on existing hubs.
fn migrate_groups(connection: &Connection) -> Result<()> {
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS groups (
           name TEXT PRIMARY KEY NOT NULL,
           description TEXT,
           lead_json TEXT NOT NULL,
           workers_json TEXT NOT NULL,
           lead_prompt TEXT,
           worker_prompt TEXT,
           created_at INTEGER NOT NULL,
           updated_at INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS group_projects (
           group_name TEXT NOT NULL REFERENCES groups(name) ON DELETE CASCADE,
           channel TEXT NOT NULL REFERENCES projects(channel),
           PRIMARY KEY (group_name, channel)
         );
         CREATE INDEX IF NOT EXISTS idx_group_projects_channel ON group_projects(channel);
         CREATE TABLE IF NOT EXISTS group_memory (
           group_name TEXT NOT NULL REFERENCES groups(name) ON DELETE CASCADE,
           kind TEXT NOT NULL CHECK(kind IN ('notes', 'scratchpad')),
           body TEXT NOT NULL,
           revision INTEGER NOT NULL CHECK(revision > 0),
           updated_at INTEGER NOT NULL,
           updated_by TEXT NOT NULL,
           PRIMARY KEY (group_name, kind)
         );",
        )
        .context("migrate groups")?;
    let exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('groups') WHERE name = 'review_policy_json')",
        [], |row| row.get(0),
    )?;
    if !exists {
        connection.execute("ALTER TABLE groups ADD COLUMN review_policy_json TEXT", [])?;
    }
    let exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('groups') WHERE name = 'inactivity_timeout_secs')",
        [], |row| row.get(0),
    )?;
    if !exists {
        connection.execute(
            "ALTER TABLE groups ADD COLUMN inactivity_timeout_secs INTEGER",
            [],
        )?;
    }
    group_activity::migrate(connection)?;
    Ok(())
}

fn migrate_projects(connection: &Connection) -> Result<()> {
    let exists: i64 = connection.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('projects') WHERE name = 'pm'",
        [],
        |row| row.get(0),
    )?;
    if exists == 0 {
        connection.execute("ALTER TABLE projects ADD COLUMN pm TEXT", [])?;
    }
    Ok(())
}

fn migrate_wake_configs(connection: &Connection) -> Result<()> {
    let columns = [
        ("session_mode", "TEXT NOT NULL DEFAULT 'fresh'"),
        ("resume_args_json", "TEXT NOT NULL DEFAULT '[]'"),
        ("session_marker", "TEXT"),
        ("session_scope", "TEXT NOT NULL DEFAULT 'account'"),
    ];

    for (column, definition) in columns {
        let exists: i64 = connection.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('wake_configs') WHERE name = ?1",
            params![column],
            |row| row.get(0),
        )?;

        if exists == 0 {
            connection.execute(
                &format!("ALTER TABLE wake_configs ADD COLUMN {column} {definition}"),
                [],
            )?;
        }
    }

    Ok(())
}

fn migrate_tasks(connection: &Connection) -> Result<()> {
    // Persist inherited pauses separately from a child's own unanswered question.
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS task_parent_blocks (
            task_id TEXT PRIMARY KEY,
            resume_status TEXT NOT NULL
        );",
    )?;
    let columns = [
        ("body", "TEXT"),
        ("kind", "TEXT NOT NULL DEFAULT 'task'"),
        ("priority", "TEXT NOT NULL DEFAULT 'normal'"),
        ("assignee", "TEXT"),
        ("created_by", "TEXT"),
        ("repo", "TEXT"),
        ("branch", "TEXT"),
        ("issue_url", "TEXT"),
        ("sprint", "TEXT"),
        ("reviewer", "TEXT"),
        ("parent_task_id", "TEXT"),
        ("review_round_json", "TEXT"),
        ("timeout_seconds", "INTEGER CHECK(timeout_seconds > 0)"),
        ("execution_heartbeat_at", "INTEGER"),
    ];

    for (column, definition) in columns {
        let exists: i64 = connection.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('tasks') WHERE name = ?1",
            params![column],
            |row| row.get(0),
        )?;

        if exists == 0 {
            connection.execute(
                &format!("ALTER TABLE tasks ADD COLUMN {column} {definition}"),
                [],
            )?;
        }
    }

    connection.execute_batch(
        "UPDATE tasks SET execution_heartbeat_at = updated_at
         WHERE status IN ('in_progress', 'testing') AND execution_heartbeat_at IS NULL;
         CREATE TRIGGER IF NOT EXISTS task_execution_phase AFTER UPDATE OF status ON tasks
         WHEN OLD.status != NEW.status
         BEGIN
           UPDATE tasks SET execution_heartbeat_at =
             CASE WHEN NEW.status IN ('in_progress', 'testing') THEN NEW.updated_at ELSE NULL END,
             lease_expires_at = CASE WHEN NEW.status = 'claimed' THEN NEW.lease_expires_at ELSE NULL END
             WHERE id = NEW.id;
         END;",
    )?;
    connection.execute(
        "CREATE INDEX IF NOT EXISTS idx_tasks_assignee ON tasks (assignee, status)",
        [],
    )?;
    connection.execute(
        "CREATE INDEX IF NOT EXISTS idx_tasks_priority ON tasks (priority, created_at)",
        [],
    )?;
    connection.execute(
        "CREATE INDEX IF NOT EXISTS idx_tasks_reviewer ON tasks (reviewer, status)",
        [],
    )?;
    connection.execute(
        "CREATE INDEX IF NOT EXISTS idx_tasks_parent ON tasks (parent_task_id)",
        [],
    )?;
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS task_dependencies (
           task_id TEXT NOT NULL,
           depends_on_task_id TEXT NOT NULL,
           created_by TEXT,
           created_at INTEGER NOT NULL,
           PRIMARY KEY (task_id, depends_on_task_id)
         );
         CREATE INDEX IF NOT EXISTS idx_task_dependencies_task
           ON task_dependencies (task_id);
         CREATE INDEX IF NOT EXISTS idx_task_dependencies_depends_on
           ON task_dependencies (depends_on_task_id);",
    )?;

    Ok(())
}

fn migrate_channel_memberships(connection: &Connection) -> Result<()> {
    let exists: i64 = connection.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('channel_memberships') WHERE name = 'last_seen_seq'",
        [],
        |row| row.get(0),
    )?;

    if exists == 0 {
        connection.execute(
            "ALTER TABLE channel_memberships ADD COLUMN last_seen_seq INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }

    Ok(())
}

fn seed_principal(
    connection: &Connection,
    id: &str,
    nick: &str,
    token: &str,
    team: &str,
) -> Result<()> {
    let inserted = connection
        .execute(
            "INSERT OR IGNORE INTO principals (id, nick, token, team, created_at)
             VALUES (?1, ?2, ?3, ?4, unixepoch())",
            params![id, nick, token, team],
        )
        .context("seed principal")?;

    // Only mirror the hardcoded demo token into `principal_tokens` when this
    // call actually created the principal row. If the principal already exists
    // its current `principals.token` may have been rotated away from the
    // hardcoded default, so hashing `token` here would write the wrong secret
    // and also block `backfill_principal_tokens` from recording the real
    // plaintext under the `migrated-legacy` label.
    if inserted == 0 {
        return Ok(());
    }

    let (_token_id, token_id_hex, _) = tokens::generate();
    let token_hash = tokens::hash(token).context("hash seed token")?;
    connection
        .execute(
            "INSERT INTO principal_tokens (principal_id, token_id, token_hash, label, created_at)
             VALUES (?1, ?2, ?3, ?4, unixepoch())",
            params![id, token_id_hex, token_hash, SEED_LABEL],
        )
        .context("seed principal_tokens row")?;

    Ok(())
}

fn migrate_principals(connection: &Connection) -> Result<()> {
    let columns = [
        ("last_seen_seq", "INTEGER NOT NULL DEFAULT 0"),
        ("connected_at", "INTEGER"),
        ("disconnected_at", "INTEGER"),
        ("role", "TEXT NOT NULL DEFAULT 'member'"),
    ];

    for (column, definition) in columns {
        let exists: i64 = connection.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('principals') WHERE name = ?1",
            params![column],
            |row| row.get(0),
        )?;

        if exists == 0 {
            connection.execute(
                &format!("ALTER TABLE principals ADD COLUMN {column} {definition}"),
                [],
            )?;
        }
    }

    Ok(())
}

/// Additive and idempotent for databases created before token scopes and TTL.
/// Existing rows default to non-expiring (`expires_at = NULL`) with `["admin"]` scope.
fn migrate_principal_tokens(connection: &Connection) -> Result<()> {
    let columns = [
        ("scopes", "TEXT NOT NULL DEFAULT '[\"admin\"]'"),
        ("ttl_seconds", "INTEGER"),
        ("expires_at", "INTEGER"),
    ];

    for (column, definition) in columns {
        let exists: i64 = connection.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('principal_tokens') WHERE name = ?1",
            params![column],
            |row| row.get(0),
        )?;

        if exists == 0 {
            connection.execute(
                &format!("ALTER TABLE principal_tokens ADD COLUMN {column} {definition}"),
                [],
            )?;
        }
    }

    connection.execute(
        "CREATE INDEX IF NOT EXISTS idx_principal_tokens_expires ON principal_tokens (expires_at)",
        [],
    )?;

    Ok(())
}

/// Mirror every legacy `principals.token` row into `principal_tokens` as a
/// `migrated-legacy` entry, keyed by a fresh random `token_id` with the
/// existing plaintext token hashed as `token_hash`. Principals that already
/// have ANY `principal_tokens` row are skipped — this covers both restart
/// idempotency and freshly seeded demo principals whose `seed` row is inserted
/// by [`seed_principal`] before this function runs. The UNIQUE
/// `(principal_id, label)` index provides a second line of defence.
fn backfill_principal_tokens(connection: &Connection) -> Result<()> {
    let mut select = connection
        .prepare(
            "SELECT p.id, p.token
             FROM principals p
             LEFT JOIN principal_tokens t ON t.principal_id = p.id
             WHERE p.token IS NOT NULL AND t.id IS NULL",
        )
        .context("prepare principal_tokens backfill query")?;

    let rows = select
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("collect principals needing backfill")?;

    for (principal_id, plaintext) in rows {
        let (_, token_id_hex, _) = tokens::generate();
        let token_hash = tokens::hash(&plaintext).context("hash legacy token for backfill")?;
        connection
            .execute(
                "INSERT INTO principal_tokens (principal_id, token_id, token_hash, label, created_at)
                 VALUES (?1, ?2, ?3, ?4, unixepoch())",
                params![principal_id, token_id_hex, token_hash, MIGRATED_LEGACY_LABEL],
            )
            .context("insert migrated-legacy principal_tokens row")?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn group_input(name: &str) -> GroupInput {
        serde_json::from_value(serde_json::json!({
            "name": name,
            "description": "Reusable team",
            "lead": {"template": "claude", "model": "opus", "nick": "dev-lead"},
            "workers": [{"template": "ollama", "model": "qwen3:35b", "count": 3}],
            "lead_prompt": "Split {task}.\nReview every result.\n",
            "worker_prompt": "Write only the assigned code.\n"
        }))
        .unwrap()
    }

    #[test]
    fn consensus_migration_reopen_and_vote_rollback_preserve_snapshots() -> Result<()> {
        use crate::tasks::{NewTask, ReviewRound, TaskStore};
        let path =
            std::env::temp_dir().join(format!("aircd-consensus-{}.sqlite3", uuid::Uuid::new_v4()));
        let saved;
        let id;
        {
            let db = open(&path)?;
            let groups = GroupStore::new(db.clone());
            let tasks = TaskStore::new(db.clone(), 60);
            groups.upsert(group_input("dev"))?;
            let task = tasks.create_full(NewTask {
                channel: "#one".into(),
                title: "Keep legacy work".into(),
                assignee: Some("worker".into()),
                ..NewTask::default()
            })?;
            id = task.id;
            {
                let connection = db.lock().unwrap();
                // Simulate the pre-consensus schema with real persisted rows.
                connection.execute("ALTER TABLE groups DROP COLUMN review_policy_json", [])?;
                connection.execute("ALTER TABLE tasks DROP COLUMN review_round_json", [])?;
                init_with(&connection, false)?;
                init_with(&connection, false)?;
            }
            assert!(groups
                .get("dev")?
                .unwrap()
                .definition
                .review_policy
                .is_none());
            assert!(tasks.get(&id)?.unwrap().review_round.is_none());
            let mut group = group_input("dev");
            group.review_policy = Some(GroupReviewPolicy {
                peer_reviewers: vec!["dev-lead".into()],
                quorum: None,
            });
            groups.upsert(group)?;
            let round = ReviewRound {
                revision: "round-1".into(),
                group: "dev".into(),
                assignee: "worker".into(),
                commit: "1".repeat(40),
                branch: "feat/consensus".into(),
                evidence: "Tests passed".into(),
                peer_reviewers: vec!["dev-lead".into()],
                quorum: 1,
                votes: Vec::new(),
            };
            tasks.set_status_with_round(
                &id,
                "assigned",
                "review",
                "worker",
                Some("Tests passed"),
                Some("feat/consensus"),
                Some(&round),
            )?;
            let before = serde_json::to_value(tasks.get(&id)?)?;
            let events = tasks.events(&id)?.len();
            db.lock().unwrap().execute_batch("CREATE TRIGGER reject_peer_event BEFORE INSERT ON task_events
                WHEN NEW.action = 'peer_review' BEGIN SELECT RAISE(ABORT, 'forced event failure'); END;")?;
            assert!(tasks
                .peer_vote(
                    &id,
                    "round-1",
                    "dev-lead",
                    "approve",
                    "Reviewed",
                    &["dev-lead".into()]
                )
                .is_err());
            assert_eq!(serde_json::to_value(tasks.get(&id)?)?, before);
            assert_eq!(tasks.events(&id)?.len(), events);
            db.lock()
                .unwrap()
                .execute("DROP TRIGGER reject_peer_event", [])?;
            saved = serde_json::to_value(tasks.peer_vote(
                &id,
                "round-1",
                "dev-lead",
                "approve",
                "Reviewed",
                &["dev-lead".into()],
            )?)?;
            assert_eq!(saved["status"], "super_review");
        }
        {
            let db = open(&path)?;
            assert!(GroupStore::new(db.clone())
                .get("dev")?
                .unwrap()
                .definition
                .review_policy
                .is_some());
            assert_eq!(
                serde_json::to_value(TaskStore::new(db, 60).get(&id)?.unwrap())?,
                saved
            );
        }
        std::fs::remove_file(path)?;
        Ok(())
    }

    fn group_project(db: &SharedConnection, channel: &str) -> Result<()> {
        crate::projects::ProjectStore::new(db.clone()).create(channel, "Project", None, "owner")?;
        Ok(())
    }

    fn membership_update(value: serde_json::Value) -> GroupMembershipUpdate {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn groups_membership_updates_attached_seats_and_preserves_other_fields() -> Result<()> {
        let db = open(":memory:")?;
        group_project(&db, "#one")?;
        let groups = GroupStore::new(db.clone());
        groups.upsert(group_input("dev"))?;
        groups.attach("dev", "#one")?;
        db.lock()
            .unwrap()
            .execute("UPDATE groups SET created_at = 10, updated_at = 20", [])?;
        let before = groups.get("dev")?.unwrap();
        let updated = groups.update_membership(
            " dev ",
            membership_update(serde_json::json!({
                "lead": {"template": "codex", "model": "default", "nick": "new-lead"},
                "remove_workers": [{"template": "ollama", "model": "qwen3:35b", "count": 2}],
                "add_workers": [
                    {"template": "ollama", "model": "qwen3:35b", "count": 3},
                    {"template": "claude", "model": "sonnet", "nick": "worker1"}
                ]
            })),
        )?;
        assert_eq!(updated.definition.workers[0].count, 4);
        assert_eq!(
            updated.definition.workers[1].nick.as_deref(),
            Some("worker1")
        );
        assert_eq!(updated.definition.lead.nick.as_deref(), Some("new-lead"));
        assert_eq!(updated.projects, before.projects);
        assert_eq!(updated.created_at, 10);
        assert_ne!(updated.updated_at, 20);
        assert_eq!(
            updated.definition.description,
            before.definition.description
        );
        assert_eq!(
            updated.definition.lead_prompt,
            before.definition.lead_prompt
        );
        assert_eq!(
            updated.definition.worker_prompt,
            before.definition.worker_prompt
        );
        let removed = groups.update_membership(
            "dev",
            membership_update(serde_json::json!({
                "remove_workers": [{"template": "claude", "model": "sonnet", "nick": "worker1"}]
            })),
        )?;
        assert_eq!(removed.definition.workers.len(), 1);
        let noop = groups.update_membership(
            "dev",
            GroupMembershipUpdate {
                lead: Some(removed.definition.lead.clone()),
                ..Default::default()
            },
        )?;
        assert_eq!(noop, removed);
        Ok(())
    }

    #[test]
    fn groups_membership_rejects_invalid_batches_without_mutation() -> Result<()> {
        let db = open(":memory:")?;
        let groups = GroupStore::new(db);
        groups.upsert(group_input("dev"))?;
        let before = groups.list()?;
        for value in [
            serde_json::json!({}),
            serde_json::json!({"lead": {"template": "c", "model": "m", "count": 2}}),
            serde_json::json!({"add_workers": [{"template": "c", "model": "m", "count": 0}]}),
            serde_json::json!({"add_workers": [{"template": "../bad", "model": "m"}]}),
            serde_json::json!({"remove_workers": [{"template": "ollama", "model": "qwen3:35b", "count": 3}]}),
            serde_json::json!({"remove_workers": [{"template": "ollama", "model": "qwen3:35b", "count": 4}]}),
            serde_json::json!({"remove_workers": [
                {"template": "ollama", "model": "qwen3:35b"},
                {"template": "ollama", "model": "qwen3:35b", "nick": "unmatched"}
            ], "lead": {"template": "c", "model": "m"}}),
            serde_json::json!({"add_workers": [
                {"template": "c", "model": "m"},
                {"template": "ollama", "model": "qwen3:35b", "count": u32::MAX}
            ]}),
        ] {
            assert!(
                groups
                    .update_membership("dev", membership_update(value.clone()))
                    .is_err(),
                "{value}"
            );
            assert_eq!(groups.list()?, before);
        }
        let error = groups
            .update_membership(
                "missing",
                membership_update(serde_json::json!({
                    "add_workers": [{"template": "c", "model": "m"}]
                })),
            )
            .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<GroupStoreError>(),
            Some(GroupStoreError::GroupNotFound(_))
        ));
        assert_eq!(groups.list()?, before);
        Ok(())
    }

    #[test]
    fn groups_membership_removes_duplicate_rows_and_replaces_last_worker() -> Result<()> {
        let groups = GroupStore::new(open(":memory:")?);
        let mut input = group_input("dev");
        input.workers.push(input.workers[0].clone());
        groups.upsert(input)?;
        let updated = groups.update_membership(
            "dev",
            membership_update(serde_json::json!({
                "remove_workers": [{"template": "ollama", "model": "qwen3:35b", "count": 6}],
                "add_workers": [{"template": "c", "model": "m"}]
            })),
        )?;
        assert_eq!(updated.definition.workers.len(), 1);
        assert_eq!(updated.definition.workers[0].template, "c");
        Ok(())
    }

    #[test]
    fn groups_membership_serializes_updates_across_connections_and_persists() -> Result<()> {
        let path =
            std::env::temp_dir().join(format!("aircd-membership-{}.sqlite3", uuid::Uuid::new_v4()));
        {
            let groups = GroupStore::new(open(&path)?);
            groups.upsert(group_input("dev"))?;
            let barrier = Arc::new(std::sync::Barrier::new(4));
            let mut threads = Vec::new();
            for _ in 0..4 {
                let db = open(&path)?;
                let barrier = barrier.clone();
                threads.push(std::thread::spawn(move || -> Result<()> {
                    let groups = GroupStore::new(db);
                    barrier.wait();
                    for _ in 0..10 {
                        groups.update_membership(
                            "dev",
                            membership_update(serde_json::json!({
                                "add_workers": [{"template": "ollama", "model": "qwen3:35b"}]
                            })),
                        )?;
                    }
                    Ok(())
                }));
            }
            for thread in threads {
                thread.join().unwrap()?;
            }
            assert_eq!(groups.get("dev")?.unwrap().definition.workers[0].count, 43);
        }
        {
            let groups = GroupStore::new(open(&path)?);
            assert_eq!(groups.get("dev")?.unwrap().definition.workers[0].count, 43);
        }
        std::fs::remove_file(path)?;
        Ok(())
    }

    #[test]
    fn groups_crud_preserves_prompts_creation_and_sorted_attachments() -> Result<()> {
        let db = open(":memory:")?;
        let groups = GroupStore::new(db.clone());
        assert!(groups.list()?.is_empty());
        groups.upsert(group_input("zeta"))?;
        let input = group_input("dev");
        let created = groups.upsert(input.clone())?;
        assert_eq!(created.definition, input);
        assert_eq!(created.definition.lead.count, 1);
        assert_eq!(groups.get(" dev ")?, Some(created.clone()));
        assert_eq!(serde_json::to_value(&created)?["name"], "dev");
        assert_eq!(groups.list()?[0], created);
        for channel in ["#two", "#one"] {
            group_project(&db, channel)?;
            groups.attach("dev", channel)?;
        }
        let attached = groups.get("dev")?.unwrap();
        assert_eq!(attached.projects, vec!["#one", "#two"]);
        assert_eq!(groups.attach("dev", "#one")?, attached);
        assert!(groups.get("zeta")?.unwrap().projects.is_empty());
        db.lock().unwrap().execute(
            "UPDATE groups SET created_at = 10, updated_at = 20 WHERE name = 'dev'",
            [],
        )?;
        let mut replacement = group_input("dev");
        replacement.description = None;
        replacement.lead_prompt = None;
        replacement.worker_prompt = Some("Replacement".to_string());
        replacement.workers[0].count = 2;
        let replaced = groups.upsert(replacement.clone())?;
        assert_eq!(replaced.definition, replacement);
        assert_eq!(replaced.created_at, 10);
        assert!(replaced.updated_at >= 20);
        assert_eq!(replaced.projects, attached.projects);
        assert!(!groups.delete("missing")?);
        assert!(groups.delete("dev")?);
        assert!(groups.get("dev")?.is_none());
        let links: i64 = db.lock().unwrap().query_row(
            "SELECT COUNT(*) FROM group_projects WHERE group_name = 'dev'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(links, 0);
        assert!(groups.upsert(group_input("dev"))?.projects.is_empty());
        assert!(crate::projects::ProjectStore::new(db)
            .get("#one")?
            .is_some());
        Ok(())
    }

    #[test]
    fn groups_reject_invalid_definitions_and_attachments_without_mutation() -> Result<()> {
        let db = open(":memory:")?;
        group_project(&db, "#one")?;
        let groups = GroupStore::new(db.clone());
        groups.upsert(group_input("dev"))?;
        groups.attach("dev", "#one")?;
        let before = groups.list()?;
        let mut invalid = Vec::new();
        for name in [
            " ",
            ".",
            "..",
            "bad/name",
            "bad\\name",
            "bad\0name",
            &"a".repeat(101),
        ] {
            invalid.push(group_input(name));
        }
        let mut input = group_input("dev");
        input.lead.count = 2;
        invalid.push(input);
        let mut input = group_input("dev");
        input.workers.clear();
        invalid.push(input);
        let mut input = group_input("dev");
        input.workers[0].count = 0;
        invalid.push(input);
        let mut input = group_input("dev");
        input.lead.template = "../bad".into();
        invalid.push(input);
        let mut input = group_input("dev");
        input.workers[0].model = " ".into();
        invalid.push(input);
        let mut input = group_input("dev");
        input.lead.nick = Some("two words".into());
        invalid.push(input);
        let mut input = group_input("dev");
        input.worker_prompt = Some("bad\0prompt".into());
        invalid.push(input);
        for input in invalid {
            assert!(groups.upsert(input).is_err());
            assert_eq!(groups.list()?, before);
        }
        for channel in ["one", "#", "#bad channel", "#bad\0channel", "#missing"] {
            assert!(groups.attach("dev", channel).is_err());
            assert_eq!(groups.list()?, before);
        }
        assert!(groups
            .attach("missing", "#one")
            .unwrap_err()
            .downcast_ref::<GroupStoreError>()
            .is_some());
        assert_eq!(groups.list()?, before);
        Ok(())
    }

    #[test]
    fn groups_migration_is_additive_idempotent_and_persistent() -> Result<()> {
        let path =
            std::env::temp_dir().join(format!("aircd-groups-{}.sqlite3", uuid::Uuid::new_v4()));
        {
            let connection = Connection::open(&path)?;
            // A project from a hub predating groups and the PM migration.
            connection.execute_batch(
                "CREATE TABLE projects (channel TEXT PRIMARY KEY, name TEXT NOT NULL,
                   description TEXT, created_by TEXT NOT NULL, created_at INTEGER NOT NULL);
                 INSERT INTO projects VALUES ('#legacy', 'Keep me', 'History', 'owner', 7);",
            )?;
            init_with(&connection, false)?;
            init_with(&connection, false)?;
            let db = SharedConnection::from_connection(connection);
            let groups = GroupStore::new(db);
            groups.upsert(group_input("dev"))?;
            groups.attach("dev", "#legacy")?;
        }
        {
            let db = open(&path)?;
            let groups = GroupStore::new(db.clone());
            assert_eq!(groups.list()?.len(), 1);
            assert_eq!(groups.get("dev")?.unwrap().definition, group_input("dev"));
            assert_eq!(groups.get("dev")?.unwrap().projects, vec!["#legacy"]);
            let project = crate::projects::ProjectStore::new(db)
                .get("#legacy")?
                .unwrap();
            assert_eq!((project.name.as_str(), project.created_at), ("Keep me", 7));
        }
        std::fs::remove_file(path)?;
        Ok(())
    }

    #[test]
    fn groups_surface_corrupt_json_without_changing_attachments() -> Result<()> {
        let db = open(":memory:")?;
        group_project(&db, "#one")?;
        let groups = GroupStore::new(db.clone());
        groups.upsert(group_input("dev"))?;
        db.lock()
            .unwrap()
            .execute("UPDATE groups SET workers_json = 'corrupt'", [])?;
        assert!(groups.get("dev").is_err());
        assert!(groups.list().is_err());
        assert!(groups.attach("dev", "#one").is_err());
        let links: i64 =
            db.lock()
                .unwrap()
                .query_row("SELECT COUNT(*) FROM group_projects", [], |row| row.get(0))?;
        assert_eq!(links, 0);
        Ok(())
    }

    fn template_input() -> ClientTemplateInput {
        ClientTemplateInput {
            name: "Example Client".to_string(),
            command: "example-client".to_string(),
            args: vec![
                "--model".to_string(),
                "{model}".to_string(),
                "{prompt}".to_string(),
            ],
            ..Default::default()
        }
    }

    fn template_store() -> Result<ClientTemplateStore> {
        let connection = Connection::open_in_memory()?;
        init_with(&connection, false)?;
        Ok(ClientTemplateStore::new(SharedConnection::from_connection(connection)))
    }

    #[test]
    fn task_workflow_migration_preserves_legacy_rows_and_round_trips_reviewers() -> Result<()> {
        let connection = Connection::open_in_memory()?;
        connection.execute_batch(
            "CREATE TABLE tasks (
                id TEXT PRIMARY KEY, channel TEXT NOT NULL, title TEXT NOT NULL,
                status TEXT NOT NULL, claimed_by TEXT, lease_expires_at INTEGER,
                created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
             );
             INSERT INTO tasks VALUES ('legacy', '#one', 'Keep me', 'claimed', 'worker', 42, 10, 20);",
        )?;
        init_with(&connection, false)?;
        init_with(&connection, false)?;
        for column in ["reviewer", "parent_task_id"] {
            let definition: (String, i64) = connection.query_row(
                "SELECT type, [notnull] FROM pragma_table_info('tasks') WHERE name = ?1",
                [column],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            assert_eq!(definition, ("TEXT".to_string(), 0));
        }
        let tasks = crate::tasks::TaskStore::new(SharedConnection::from_connection(connection), 60);
        let legacy = tasks.get("legacy")?.unwrap();
        assert_eq!(legacy.title, "Keep me");
        assert_eq!(legacy.status, "claimed");
        assert_eq!(legacy.claimed_by.as_deref(), Some("worker"));
        assert_eq!(legacy.lease_expires_at, Some(42));
        assert_eq!((legacy.created_at, legacy.updated_at), (10, 20));
        assert!(legacy.reviewer.is_none());
        assert!(legacy.parent_task_id.is_none());
        assert_eq!(legacy.priority, "normal");

        let child = tasks.create_full(crate::tasks::NewTask {
            channel: "#one".to_string(),
            title: "Child".to_string(),
            priority: Some("high".to_string()),
            assignee: Some("worker".to_string()),
            reviewer: Some("lead".to_string()),
            parent_task_id: Some("legacy".to_string()),
            ..Default::default()
        })?;
        let reloaded = tasks.get(&child.id)?.unwrap();
        assert_eq!(reloaded.priority, "high");
        assert_eq!(reloaded.reviewer.as_deref(), Some("lead"));
        assert_eq!(reloaded.parent_task_id.as_deref(), Some("legacy"));
        let worker_inbox = tasks.inbox("worker")?;
        assert_eq!(worker_inbox.len(), 2);
        assert_eq!(worker_inbox[0].id, child.id);
        assert_eq!(worker_inbox[1].id, "legacy");
        tasks.set_status(
            &child.id,
            "assigned",
            "review",
            "worker",
            Some("tests pass"),
            None,
        )?;
        assert_eq!(
            tasks.review_queue("#one")?[0].reviewer.as_deref(),
            Some("lead")
        );
        Ok(())
    }

    #[test]
    fn task_priority_queue_orders_inbox_list_and_review_queue() -> Result<()> {
        let connection = Connection::open_in_memory()?;
        init_with(&connection, false)?;
        let tasks = crate::tasks::TaskStore::new(SharedConnection::from_connection(connection), 60);

        let low = tasks.create_full(crate::tasks::NewTask {
            channel: "#alpha".into(),
            title: "Low task".into(),
            priority: Some("low".into()),
            assignee: Some("worker".into()),
            ..Default::default()
        })?;
        let normal1 = tasks.create_full(crate::tasks::NewTask {
            channel: "#alpha".into(),
            title: "Normal 1".into(),
            assignee: Some("worker".into()),
            ..Default::default()
        })?;
        let high = tasks.create_full(crate::tasks::NewTask {
            channel: "#beta".into(),
            title: "High task".into(),
            priority: Some("HIGH".into()),
            assignee: Some("worker".into()),
            ..Default::default()
        })?;
        let urgent = tasks.create_full(crate::tasks::NewTask {
            channel: "#gamma".into(),
            title: "Urgent task".into(),
            priority: Some("urgent".into()),
            assignee: Some("worker".into()),
            ..Default::default()
        })?;
        let normal2 = tasks.create_full(crate::tasks::NewTask {
            channel: "#alpha".into(),
            title: "Normal 2".into(),
            priority: Some("normal".into()),
            assignee: Some("worker".into()),
            ..Default::default()
        })?;

        assert!(tasks
            .create_full(crate::tasks::NewTask {
                channel: "#alpha".into(),
                title: "Bad priority".into(),
                priority: Some("critical".into()),
                ..Default::default()
            })
            .is_err());

        let inbox_ids: Vec<_> = tasks
            .inbox("worker")?
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(
            inbox_ids,
            vec![
                urgent.id.clone(),
                high.id.clone(),
                normal1.id.clone(),
                normal2.id.clone(),
                low.id.clone()
            ]
        );

        let alpha_ids: Vec<_> = tasks
            .list("#alpha")?
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(
            alpha_ids,
            vec![normal1.id.clone(), normal2.id.clone(), low.id.clone()]
        );

        let (bumped, changed) = tasks.set_priority(&low.id, "urgent", "pm")?;
        assert!(changed);
        assert_eq!(bumped.priority, "urgent");
        let (_, changed_again) = tasks.set_priority(&low.id, "urgent", "pm")?;
        assert!(!changed_again);

        let inbox_after: Vec<_> = tasks
            .inbox("worker")?
            .into_iter()
            .map(|t| t.id)
            .collect();
        // Both low (now urgent, created earlier) and urgent share urgent priority, ordered FIFO.
        assert_eq!(
            inbox_after,
            vec![
                low.id.clone(),
                urgent.id.clone(),
                high.id.clone(),
                normal1.id.clone(),
                normal2.id.clone()
            ]
        );
        Ok(())
    }

    #[test]
    fn task_dependencies_store_enforces_cross_repo_dag_and_blocked_by_validation() -> Result<()> {
        let connection = Connection::open_in_memory()?;
        init_with(&connection, false)?;
        // Idempotent re-init keeps task_dependencies intact.
        init_with(&connection, false)?;
        let table_exists: i64 = connection.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'task_dependencies'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(table_exists, 1);

        let tasks = crate::tasks::TaskStore::new(SharedConnection::from_connection(connection), 60);

        let t_backend = tasks.create_full(crate::tasks::NewTask {
            channel: "#api".into(),
            title: "Build backend schema".into(),
            repo: Some("backend".into()),
            assignee: Some("alice".into()),
            ..Default::default()
        })?;
        let t_frontend = tasks.create_full(crate::tasks::NewTask {
            channel: "#web".into(),
            title: "Integrate frontend UI".into(),
            repo: Some("frontend".into()),
            assignee: Some("bob".into()),
            blocked_by: vec![t_backend.id.clone(), format!("  {}  ", t_backend.id)],
            ..Default::default()
        })?;
        assert_eq!(t_frontend.blocked_by, vec![t_backend.id.clone()]);

        // Missing dependency rejected.
        assert!(tasks
            .create_full(crate::tasks::NewTask {
                channel: "#web".into(),
                title: "Bad dep".into(),
                blocked_by: vec!["task_missing".into()],
                ..Default::default()
            })
            .is_err());

        // Self-dependency and cycles rejected.
        assert!(tasks
            .add_dependencies(&t_backend.id, std::slice::from_ref(&t_backend.id), "pm")
            .is_err());
        let cycle_err = tasks
            .add_dependencies(&t_backend.id, std::slice::from_ref(&t_frontend.id), "pm")
            .unwrap_err();
        assert!(cycle_err.to_string().contains("circular"));

        // 3-node transitive cycle rejected: t_mid -> t_frontend -> t_backend, then t_backend -> t_mid.
        let t_mid = tasks.create_full(crate::tasks::NewTask {
            channel: "#sdk".into(),
            title: "Publish SDK".into(),
            repo: Some("sdk".into()),
            blocked_by: vec![t_frontend.id.clone()],
            ..Default::default()
        })?;
        assert!(tasks
            .set_dependencies(&t_backend.id, std::slice::from_ref(&t_mid.id), "pm")
            .is_err());

        // Cross-repo and cross-project flags populated.
        let deps = tasks.dependencies(&t_frontend.id)?;
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].task_id, t_backend.id);
        assert_eq!(deps[0].repo.as_deref(), Some("backend"));
        assert!(!deps[0].resolved);
        assert!(deps[0].cross_repo);
        assert!(deps[0].cross_project);

        let rev = tasks.dependents(&t_backend.id)?;
        assert_eq!(rev.len(), 1);
        assert_eq!(rev[0].task_id, t_frontend.id);

        // Blocked task cannot advance to in_progress/testing/review/merged, nor be claimed.
        let blocked_err = tasks
            .set_status(&t_frontend.id, "assigned", "in_progress", "bob", None, None)
            .unwrap_err();
        assert!(blocked_err.to_string().contains(&t_backend.id));

        let t_claimable = tasks.create_full(crate::tasks::NewTask {
            channel: "#web".into(),
            title: "Claimable after backend".into(),
            repo: Some("frontend".into()),
            blocked_by: vec![t_backend.id.clone()],
            ..Default::default()
        })?;
        assert!(tasks.claim(&t_claimable.id, "bob").is_err());

        // Filtering by repo and blocked state works.
        assert_eq!(
            tasks
                .list_filtered_with_options("#web", None, Some("frontend"), Some(true))?
                .len(),
            2
        );
        assert!(tasks
            .list_filtered_with_options("#web", None, Some("frontend"), Some(false))?
            .is_empty());

        // Completing the blocker unblocks dependents.
        tasks.set_status(&t_backend.id, "assigned", "merged", "pm", Some("ok"), None)?;
        assert!(tasks.unresolved_blockers(&t_frontend.id)?.is_empty());
        let advanced = tasks.set_status(&t_frontend.id, "assigned", "in_progress", "bob", None, None)?;
        assert_eq!(advanced.status, "in_progress");
        let claimed = tasks.claim(&t_claimable.id, "bob")?;
        assert_eq!(claimed.status, "claimed");

        // Cancelling a blocker keeps dependents blocked unless unlinked, while cancelling a blocked task is allowed.
        let t_to_cancel = tasks.create_full(crate::tasks::NewTask {
            channel: "#api".into(),
            title: "Cancelled prerequisite".into(),
            ..Default::default()
        })?;
        let t_waiting = tasks.create_full(crate::tasks::NewTask {
            channel: "#web".into(),
            title: "Waiting on cancelled".into(),
            assignee: Some("bob".into()),
            blocked_by: vec![t_to_cancel.id.clone()],
            ..Default::default()
        })?;
        tasks.set_status(&t_to_cancel.id, "open", "cancelled", "pm", Some("dropped"), None)?;
        assert!(tasks
            .set_status(&t_waiting.id, "assigned", "in_progress", "bob", None, None)
            .is_err());
        let (unlinked, removed) = tasks.remove_dependency(&t_waiting.id, &t_to_cancel.id, "pm")?;
        assert!(removed);
        assert!(unlinked.blocked_by.is_empty());
        assert_eq!(
            tasks
                .set_status(&t_waiting.id, "assigned", "in_progress", "bob", None, None)?
                .status,
            "in_progress"
        );
        Ok(())
    }

    #[test]
    fn client_templates_round_trip_launcher_settings_verbatim() -> Result<()> {
        let store = template_store()?;
        let input = ClientTemplateInput {
            name: "Example \"Client\" — 日本語".to_string(),
            command: r"C:\Agent Tools\example.exe".to_string(),
            args: vec![
                "--model={model}".to_string(),
                "{prompt}".to_string(),
                "".to_string(),
                "O'Brien says \"hello\"\n$literal `text`".to_string(),
            ],
            cwd: Some("{repo_path}".to_string()),
            env: BTreeMap::from([
                (
                    "KEEP".to_string(),
                    Some("quote=\"; newline\n{model}".to_string()),
                ),
                ("EMPTY".to_string(), Some(String::new())),
                ("REMOVE".to_string(), None),
            ]),
            prompt: Some("Start\n{nick}: {reason}".to_string()),
            session_mode: Some("resume".to_string()),
            resume_args: vec![
                "resume".to_string(),
                "{session_id}".to_string(),
                "{model}".to_string(),
            ],
            session_marker: Some("\"session_id\":".to_string()),
            session_scope: Some("channel".to_string()),
            models: vec!["model-a".to_string(), "model.b_2".to_string()],
            models_command: vec!["example-client".to_string(), "models".to_string()],
        };
        let saved = store.upsert("example", input.clone())?;
        assert_eq!(saved.models, input.models);
        assert_eq!(saved.models_command, input.models_command);
        assert_eq!(saved.name, input.name);
        assert_eq!(saved.command, input.command);
        assert_eq!(saved.args, input.args);
        assert_eq!(saved.cwd, input.cwd);
        assert_eq!(saved.env, input.env);
        assert_eq!(saved.prompt, input.prompt);
        assert_eq!(saved.session_mode, "resume");
        assert_eq!(saved.resume_args, input.resume_args);
        assert_eq!(saved.session_marker, input.session_marker);
        assert_eq!(saved.session_scope, "channel");
        assert!(saved.created_at > 0);
        assert_eq!(saved.created_at, saved.updated_at);
        assert_eq!(store.get("example")?, Some(saved.clone()));
        assert_eq!(store.list()?, vec![saved]);
        Ok(())
    }

    #[test]
    fn client_templates_defaults_and_optional_values_match_launcher() -> Result<()> {
        let store = template_store()?;
        let input = serde_json::from_value(serde_json::json!({
            "name": "  Example Client  ", "command": "  example-client  ",
            "cwd": "  ", "prompt": "\n", "session_marker": " ",
            "session_mode": " ", "session_scope": " "
        }))?;
        let saved = store.upsert(" example ", input)?;
        assert_eq!(saved.id, "example");
        assert_eq!(saved.name, "Example Client");
        assert_eq!(saved.command, "example-client");
        assert_eq!(saved.session_mode, "fresh");
        assert_eq!(saved.session_scope, "account");
        assert!(saved.args.is_empty());
        assert!(saved.resume_args.is_empty());
        assert!(saved.env.is_empty());
        assert_eq!(saved.cwd, None);
        assert_eq!(saved.prompt, None);
        assert_eq!(saved.session_marker, None);
        assert_eq!(store.get(" example ")?, Some(saved));
        let defaulted = store.upsert("minimal", template_input())?;
        assert_eq!(defaulted.session_mode, "fresh");
        assert_eq!(defaulted.session_scope, "account");
        Ok(())
    }

    #[test]
    fn client_templates_update_replaces_fields_and_keeps_creation_time() -> Result<()> {
        let store = template_store()?;
        let mut input = template_input();
        input.env.insert("REMOVE".to_string(), None);
        input.resume_args.push("{session_id}".to_string());
        input.session_mode = Some("resume".to_string());
        store.upsert("example", input)?;
        store.db.lock().unwrap().execute(
            "UPDATE client_templates SET created_at = 1, updated_at = 2 WHERE id = 'example'",
            [],
        )?;
        let mut replacement = template_input();
        replacement.name = "Renamed".to_string();
        replacement.command = "new-client".to_string();
        replacement.args.clear();
        let updated = store.upsert("example", replacement)?;
        assert_eq!(updated.created_at, 1);
        assert!(updated.updated_at > 2);
        assert_eq!(updated.name, "Renamed");
        assert_eq!(updated.command, "new-client");
        assert!(updated.env.is_empty());
        assert!(updated.args.is_empty());
        assert!(updated.resume_args.is_empty());
        assert_eq!(updated.session_mode, "fresh");
        assert_eq!(store.list()?.len(), 1);
        assert_eq!(store.get("example")?, Some(updated));
        Ok(())
    }

    #[test]
    fn client_templates_directory_and_delete_are_deterministic() -> Result<()> {
        let store = template_store()?;
        assert!(store.list()?.is_empty());
        assert_eq!(store.get("missing")?, None);
        assert!(!store.delete("missing")?);
        for id in ["zeta", "alpha", "middle"] {
            store.upsert(id, template_input())?;
        }
        let ids: Vec<_> = store.list()?.into_iter().map(|row| row.id).collect();
        assert_eq!(ids, ["alpha", "middle", "zeta"]);
        assert!(store.delete(" middle ")?);
        assert!(!store.delete("middle")?);
        assert_eq!(store.get("middle")?, None);
        assert_eq!(store.list()?.len(), 2);
        Ok(())
    }

    #[test]
    fn client_templates_reject_invalid_input_without_mutation() -> Result<()> {
        let store = template_store()?;
        let original = store.upsert("example", template_input())?;
        for id in ["", " ", "two words", "../example", "example\0", "é"] {
            assert!(store.upsert(id, template_input()).is_err(), "{id:?}");
            assert!(store.get(id).is_err(), "{id:?}");
            assert!(store.delete(id).is_err(), "{id:?}");
        }
        for (field, value) in [
            ("name", " "),
            ("command", "\n"),
            ("name", "nul\0"),
            ("command", "nul\0"),
            ("session_mode", "unknown"),
            ("session_scope", "unknown"),
        ] {
            let mut input = template_input();
            match field {
                "name" => input.name = value.to_string(),
                "command" => input.command = value.to_string(),
                "session_mode" => input.session_mode = Some(value.to_string()),
                "session_scope" => input.session_scope = Some(value.to_string()),
                _ => unreachable!(),
            }
            assert!(store.upsert("example", input).is_err(), "{field}");
            assert_eq!(store.get("example")?, Some(original.clone()));
        }
        assert_eq!(store.list()?, vec![original]);
        Ok(())
    }

    #[test]
    fn client_templates_support_all_launcher_session_modes() -> Result<()> {
        let store = template_store()?;
        for (mode, scope) in [
            ("fresh", "account"),
            ("resume", "channel"),
            ("pm", "channel"),
        ] {
            let mut input = template_input();
            input.session_mode = Some(format!(" {mode} ").to_uppercase());
            input.session_scope = Some(format!(" {scope} ").to_uppercase());
            let saved = store.upsert("example", input)?;
            assert_eq!(saved.session_mode, mode);
            assert_eq!(saved.session_scope, scope);
        }
        Ok(())
    }

    #[test]
    fn client_template_launcher_fills_in_the_model_only() -> Result<()> {
        let store = template_store()?;
        let mut input = template_input();
        input.env = BTreeMap::from([
            ("MODEL".to_string(), Some("{model}".to_string())),
            ("DROP".to_string(), None),
        ]);
        input.prompt = Some("You are @{nick} on {model}".to_string());
        input.session_mode = Some("pm".to_string());
        input.resume_args = vec!["--model".into(), "{model}".into(), "{session_id}".into()];
        input.session_marker = Some("conversation_id".to_string());
        let template = store.upsert("example", input)?;
        let launcher = template.launcher_for("big-model-high");
        assert_eq!(launcher.command, "example-client");
        assert_eq!(launcher.args, ["--model", "big-model-high", "{prompt}"]);
        assert_eq!(launcher.resume_args, ["--model", "big-model-high", "{session_id}"]);
        assert_eq!(launcher.env["MODEL"].as_deref(), Some("big-model-high"));
        assert_eq!(launcher.env["DROP"], None);
        assert_eq!(launcher.prompt.as_deref(), Some("You are @{nick} on big-model-high"));
        assert_eq!(launcher.session_mode.as_deref(), Some("pm"));
        assert_eq!(launcher.session_marker.as_deref(), Some("conversation_id"));
        assert_eq!(launcher.enabled, Some(true));
        Ok(())
    }

    #[test]
    fn client_template_models_are_trimmed_deduplicated_and_checked() -> Result<()> {
        let store = template_store()?;
        let mut input = template_input();
        input.models = vec![" a-1 ".into(), "b.2".into(), "a-1".into()];
        assert_eq!(store.upsert("example", input)?.models, ["a-1", "b.2"]);
        for bad in ["", "two words", "{model}", "tab\tbed"] {
            let mut input = template_input();
            input.models = vec![bad.to_string()];
            assert!(store.upsert("bad", input).is_err(), "{bad:?}");
        }
        assert!(store.get("bad")?.is_none());
        Ok(())
    }

    #[test]
    fn client_templates_from_before_model_lists_gain_empty_lists() -> Result<()> {
        let connection = Connection::open_in_memory()?;
        // The first template table, without model columns.
        connection.execute_batch(
            "CREATE TABLE client_templates (
               id TEXT PRIMARY KEY NOT NULL, name TEXT NOT NULL, command TEXT NOT NULL,
               args_json TEXT NOT NULL DEFAULT '[]', cwd TEXT,
               env_json TEXT NOT NULL DEFAULT '{}', prompt TEXT,
               session_mode TEXT NOT NULL DEFAULT 'fresh',
               resume_args_json TEXT NOT NULL DEFAULT '[]', session_marker TEXT,
               session_scope TEXT NOT NULL DEFAULT 'account',
               created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
             );
             INSERT INTO client_templates (id, name, command, created_at, updated_at)
               VALUES ('old', 'Old', 'old-client', 5, 6);",
        )?;
        init_with(&connection, false)?;
        init_with(&connection, false)?;
        let store = ClientTemplateStore::new(SharedConnection::from_connection(connection));
        let old = store.get("old")?.unwrap();
        assert!(old.models.is_empty() && old.models_command.is_empty());
        assert_eq!((old.created_at, old.updated_at), (5, 6));
        Ok(())
    }

    #[test]
    fn client_templates_migrate_existing_database_without_changing_launchers() -> Result<()> {
        let connection = Connection::open_in_memory()?;
        // A database from before template storage and session-aware launchers.
        connection.execute_batch(
            "CREATE TABLE wake_configs (
               principal_id TEXT PRIMARY KEY, command TEXT NOT NULL,
               args_json TEXT NOT NULL, cwd TEXT, env_json TEXT NOT NULL, prompt TEXT,
               enabled INTEGER NOT NULL DEFAULT 1, cooldown_seconds INTEGER NOT NULL,
               timeout_seconds INTEGER NOT NULL, max_runs_per_hour INTEGER NOT NULL,
               updated_at INTEGER NOT NULL
             );
             INSERT INTO wake_configs VALUES
               ('existing-worker', 'existing-command', '[\"{prompt}\"]', NULL, '{}',
                NULL, 1, 20, 1800, 12, 123);",
        )?;
        init_with(&connection, false)?;
        let store = ClientTemplateStore::new(SharedConnection::from_connection(connection));
        assert!(
            store.list()?.is_empty(),
            "templates are defined by the owner"
        );
        let saved = store.upsert("example", template_input())?;
        {
            let db = store.db.lock().unwrap();
            init_with(&db, false)?;
            init_with(&db, false)?;
        }
        assert_eq!(store.get("example")?, Some(saved));
        let wake = crate::wake::WakeManager::new(store.db.clone());
        let existing = wake.get("existing-worker")?.unwrap();
        assert_eq!(existing.command, "existing-command");
        assert_eq!(existing.args, ["{prompt}"]);
        assert_eq!(existing.updated_at, 123);
        assert_eq!(existing.session_mode, "fresh");
        assert_eq!(existing.session_scope, "account");
        assert!(store.delete("example")?);
        assert!(wake.get("existing-worker")?.is_some());
        Ok(())
    }

    #[test]
    fn client_templates_survive_database_reopen() -> Result<()> {
        struct TestDb(std::path::PathBuf);
        impl Drop for TestDb {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let file = TestDb(std::env::temp_dir().join(format!(
            "aircd-client-templates-{}.sqlite3",
            uuid::Uuid::new_v4()
        )));
        let saved = {
            let store = ClientTemplateStore::new(open(&file.0)?);
            store.upsert("example", template_input())?
        };
        {
            let store = ClientTemplateStore::new(open(&file.0)?);
            assert_eq!(store.get("example")?, Some(saved));
            assert!(store.delete("example")?);
        }
        let store = ClientTemplateStore::new(open(&file.0)?);
        assert!(store.list()?.is_empty());
        Ok(())
    }

    #[test]
    fn client_templates_report_corrupt_json_instead_of_losing_settings() -> Result<()> {
        let store = template_store()?;
        for (column, value) in [
            ("args_json", "invalid"),
            ("env_json", "[]"),
            ("resume_args_json", "{}"),
        ] {
            store.upsert("example", template_input())?;
            store.db.lock().unwrap().execute(
                &format!("UPDATE client_templates SET {column} = ?1 WHERE id = 'example'"),
                params![value],
            )?;
            assert!(store.get("example").is_err(), "{column}");
            assert!(store.list().is_err(), "{column}");
        }
        Ok(())
    }

    #[test]
    fn published_demo_tokens_stop_working_on_a_real_database() -> Result<()> {
        let connection = Connection::open_in_memory()?;
        // A database created by an older build: demo principals are present.
        init_with(&connection, true)?;
        // Reopened by a locked-down build.
        init_with(&connection, false)?;
        init_with(&connection, false)?; // idempotent

        let shared: SharedConnection = SharedConnection::from_connection(connection);
        let auth = crate::auth::AuthStore::new(shared.clone());
        for (_id, token, _team) in DEMO_PRINCIPALS {
            assert!(
                auth.authenticate(token).is_err(),
                "{token} must be rejected"
            );
        }
        let kept: i64 = shared.lock().unwrap().query_row(
            "SELECT COUNT(*) FROM principals WHERE id = 'human'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(kept, 1, "account name and history are kept");
        Ok(())
    }

    #[test]
    fn init_creates_principal_tokens_table_and_seeds_mirrors() -> Result<()> {
        let connection = Connection::open_in_memory()?;
        init(&connection)?;

        let seed_count: i64 = connection.query_row(
            "SELECT COUNT(*) FROM principal_tokens WHERE label = ?1",
            params![SEED_LABEL],
            |row| row.get(0),
        )?;
        assert_eq!(seed_count, 7, "every seeded principal should have a row");

        let unique_ids: i64 = connection.query_row(
            "SELECT COUNT(DISTINCT token_id) FROM principal_tokens",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(unique_ids, seed_count);
        Ok(())
    }

    #[test]
    fn backfill_is_idempotent_across_reinit() -> Result<()> {
        let connection = Connection::open_in_memory()?;
        init(&connection)?;
        let first: i64 =
            connection.query_row("SELECT COUNT(*) FROM principal_tokens", [], |row| {
                row.get(0)
            })?;

        // Simulate a legacy principal inserted before principal_tokens existed.
        connection.execute(
            "INSERT INTO principals (id, nick, token, team, created_at)
             VALUES ('legacy', 'legacy', 'legacy-plaintext', 'ops', unixepoch())",
            [],
        )?;

        init(&connection)?;
        let second: i64 =
            connection.query_row("SELECT COUNT(*) FROM principal_tokens", [], |row| {
                row.get(0)
            })?;
        assert_eq!(second, first + 1, "one legacy row should be backfilled");

        init(&connection)?;
        let third: i64 =
            connection.query_row("SELECT COUNT(*) FROM principal_tokens", [], |row| {
                row.get(0)
            })?;
        assert_eq!(third, second, "backfill must not duplicate rows");

        let (label, hash): (String, String) = connection.query_row(
            "SELECT label, token_hash FROM principal_tokens WHERE principal_id = 'legacy'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(label, MIGRATED_LEGACY_LABEL);
        assert!(tokens::verify("legacy-plaintext", &hash)?);
        Ok(())
    }

    #[test]
    fn reinit_preserves_rotated_plaintext_via_backfill() -> Result<()> {
        // Simulate a pre-existing DB where a built-in principal's legacy token
        // was rotated away from the hardcoded seed default before
        // principal_tokens existed. `init` on such a DB must not silently
        // hash the stale hardcoded default — it must hash the actual current
        // `principals.token` via the migrated-legacy backfill.
        let connection = Connection::open_in_memory()?;
        connection.execute_batch(
            "CREATE TABLE principals (
               id TEXT PRIMARY KEY,
               nick TEXT NOT NULL,
               token TEXT UNIQUE NOT NULL,
               team TEXT,
               last_seen_seq INTEGER NOT NULL DEFAULT 0,
               connected_at INTEGER,
               disconnected_at INTEGER,
               created_at INTEGER NOT NULL
             );",
        )?;
        connection.execute(
            "INSERT INTO principals (id, nick, token, team, created_at)
             VALUES ('human', 'human', 'rotated-secret', 'demo', unixepoch())",
            [],
        )?;

        init(&connection)?;

        let (label, hash): (String, String) = connection.query_row(
            "SELECT label, token_hash FROM principal_tokens WHERE principal_id = 'human'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(
            label, MIGRATED_LEGACY_LABEL,
            "pre-existing principal with rotated token must take the migrated-legacy backfill path, not the seed path"
        );
        assert!(
            tokens::verify("rotated-secret", &hash)?,
            "backfill must hash the actual current plaintext, not the hardcoded default"
        );
        assert!(
            !tokens::verify("human-token", &hash)?,
            "the hardcoded seed default must not be written for a pre-existing principal"
        );
        Ok(())
    }

    #[test]
    fn init_creates_extended_tables_and_role_column() -> Result<()> {
        let connection = Connection::open_in_memory()?;
        init(&connection)?;

        for table in [
            "webhooks",
            "webhook_deliveries",
            "reports",
            "git_repos",
            "resource_locks",
        ] {
            let exists: i64 = connection.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                params![table],
                |row| row.get(0),
            )?;
            assert_eq!(exists, 1, "table {table} must exist after init");
        }

        let role: String = connection.query_row(
            "SELECT role FROM principals WHERE id = 'human'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(role, "member");
        Ok(())
    }

    #[test]
    fn migration_adds_token_scope_and_ttl_columns_with_safe_defaults() -> Result<()> {
        let connection = Connection::open_in_memory()?;
        connection.execute_batch(
            "CREATE TABLE principals (
               id TEXT PRIMARY KEY,
               nick TEXT NOT NULL,
               token TEXT UNIQUE NOT NULL,
               team TEXT,
               created_at INTEGER NOT NULL
             );
             CREATE TABLE principal_tokens (
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               principal_id TEXT NOT NULL REFERENCES principals(id) ON DELETE CASCADE,
               token_id TEXT UNIQUE NOT NULL,
               token_hash TEXT NOT NULL,
               label TEXT,
               created_at INTEGER NOT NULL,
               revoked_at INTEGER
             );
             INSERT INTO principals (id, nick, token, team, created_at)
             VALUES ('legacy-user', 'legacy-user', 'legacy-secret', 'core', 100);
             INSERT INTO principal_tokens (principal_id, token_id, token_hash, label, created_at)
             VALUES ('legacy-user', 'toklegacy1', 'dummyhash', 'cli', 100);",
        )?;

        // Running init_with twice verifies additive migration idempotency.
        init_with(&connection, false)?;
        init_with(&connection, false)?;

        let (scopes, ttl_seconds, expires_at): (String, Option<i64>, Option<i64>) = connection
            .query_row(
                "SELECT scopes, ttl_seconds, expires_at FROM principal_tokens WHERE token_id = 'toklegacy1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
        assert_eq!(scopes, r#"["admin"]"#);
        assert_eq!(ttl_seconds, None);
        assert_eq!(expires_at, None);
        Ok(())
    }
}
