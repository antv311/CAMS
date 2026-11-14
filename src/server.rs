use anyhow::{anyhow, Context, Result};
use rusqlite::params;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio_rustls::TlsAcceptor;

use crate::attachments::{AttachmentStore, NewAttachment, TaskAttachment};
use crate::auth::{AuthStore, AuthTokenInfo, IssuedToken, Principal, Scope};
use crate::backup::{BackupConfig, BackupManager};
use crate::gits::{sanitize_repo_name, GitRepo, GitRepoStore};
use crate::history::{canonical_dm_channel, unix_timestamp, HistoryStore, StoredMessage};
use crate::locks::{LockConflictError, LockStore, ResourceLock};
use crate::logging::{EventForwarder, LogEvent, Severity};
use crate::notes::NoteStore;
use crate::projects::{Project, ProjectMember, ProjectStore, CHAT_ONLY_CHANNEL, MANAGER_ROLES};
use crate::protocol::{parse_command, Command};
use crate::reports::{NewReport, Report, ReportStore};
use crate::rooms::{Room, RoomStore};
use crate::store::{ClientTemplate, ClientTemplateStore, GroupStore, SharedConnection};
use crate::tasks::{NewTask, ReviewRound, Task, TaskStore, TERMINAL_STATUSES};
use crate::wake::{WakeConfig, WakeContext, WakeManager};
use crate::webhooks::{WebhookEvent, WebhookStore};

/// An account made from a client template. `token` is shown once.
#[derive(Debug, Serialize)]
pub struct CreatedAccount {
    pub principal: Principal,
    pub token: String,
    pub channel: String,
    pub channels: Vec<String>,
    pub role: String,
    pub model: String,
    pub template: String,
    pub wake: WakeConfig,
}

#[derive(Debug)]
pub(crate) enum SpawnError {
    Forbidden(String),
    TemplateNotFound(String),
}

impl std::fmt::Display for SpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Forbidden(message) | Self::TemplateNotFound(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for SpawnError {}

const SERVER_NAME: &str = "aircd";
const METADATA_CAPABILITIES: &[&str] = &["message-tags", "echo-message"];

#[derive(Clone)]
pub struct Server {
    db: SharedConnection,
    auth: AuthStore,
    history: HistoryStore,
    tasks: TaskStore,
    webhooks: WebhookStore,
    reports: ReportStore,
    gits: GitRepoStore,
    locks: LockStore,
    projects: ProjectStore,
    wakes: WakeManager,
    notes: NoteStore,
    attachments: AttachmentStore,
    rooms: RoomStore,
    templates: ClientTemplateStore,
    groups: GroupStore,
    backups: BackupManager,
    logger: EventForwarder,
    // Lock before db/inner: registration, command effects, reissue and cleanup
    // must agree on which session owns a principal across credential changes.
    session_gate: Arc<Mutex<()>>,
    task_gate: Arc<Mutex<()>>,
    inner: Arc<Mutex<ServerInner>>,
}

#[derive(Clone, Debug, Serialize)]
pub struct AgentDirectoryEntry {
    pub id: String,
    pub nick: String,
    pub team: Option<String>,
    pub role: String,
    pub online: bool,
    pub webhook_configured: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct ChannelSummary {
    pub channel: String,
    pub message_count: i64,
    pub last_activity_at: Option<i64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct DirectorySnapshot {
    pub agents: Vec<AgentDirectoryEntry>,
    pub channels: Vec<ChannelSummary>,
    pub git_repos: Vec<GitRepo>,
    pub locks: Vec<ResourceLock>,
}

#[derive(Default)]
struct ServerInner {
    clients: HashMap<String, ClientHandle>,
    channels: HashMap<String, HashSet<String>>,
}

#[derive(Clone)]
struct ClientHandle {
    session_id: String,
    tx: UnboundedSender<String>,
    shutdown_tx: UnboundedSender<String>,
}

#[derive(Clone, Debug)]
struct Membership {
    channel: String,
    last_seen_seq: i64,
}

struct Session {
    session_id: String,
    principal: Option<Principal>,
    token_info: Option<AuthTokenInfo>,
    nick: Option<String>,
    pass_token: Option<String>,
    registered: bool,
    capabilities: HashSet<String>,
    tx: UnboundedSender<String>,
    shutdown_tx: UnboundedSender<String>,
}

impl Session {
    fn has_scope(&self, required: Scope) -> bool {
        self.token_info
            .as_ref()
            .is_some_and(|info| info.has_scope(required))
    }
}

impl Server {
    #[allow(dead_code)]
    pub fn new(db: SharedConnection) -> Self {
        Self::with_logger(db, EventForwarder::default())
    }

    pub fn with_logger(db: SharedConnection, logger: EventForwarder) -> Self {
        Self::with_logger_and_backup(db, logger, BackupConfig::default())
    }

    pub fn with_logger_and_backup(
        db: SharedConnection,
        logger: EventForwarder,
        backup_config: BackupConfig,
    ) -> Self {
        Self::with_logger_backup_and_ttl(db, logger, backup_config, None)
    }

    pub fn with_logger_backup_and_ttl(
        db: SharedConnection,
        logger: EventForwarder,
        backup_config: BackupConfig,
        token_ttl_seconds: Option<u64>,
    ) -> Self {
        let lease_seconds = std::env::var("AIRCD_TASK_LEASE_SECONDS")
            .ok()
            .and_then(|value| value.parse::<i64>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(300);

        let server = Self {
            auth: AuthStore::with_config(db.clone(), logger.clone(), token_ttl_seconds),
            history: HistoryStore::new(db.clone()),
            tasks: TaskStore::new(db.clone(), lease_seconds),
            webhooks: WebhookStore::with_logger(db.clone(), logger.clone()),
            reports: ReportStore::new(db.clone()),
            gits: GitRepoStore::new(db.clone()),
            locks: LockStore::new(db.clone()),
            projects: ProjectStore::new(db.clone()),
            wakes: WakeManager::with_logger(db.clone(), logger.clone()),
            notes: NoteStore::new(db.clone()),
            attachments: AttachmentStore::new(db.clone()),
            rooms: RoomStore::new(db.clone()),
            templates: ClientTemplateStore::new(db.clone()),
            groups: GroupStore::new(db.clone()),
            backups: BackupManager::new(db.clone(), backup_config, logger.clone()),
            logger: logger.clone(),
            db,
            session_gate: Arc::new(Mutex::new(())),
            task_gate: Arc::new(Mutex::new(())),
            inner: Arc::new(Mutex::new(ServerInner::default())),
        };
        // The poster gets its own idle launcher so the server's launcher does
        // not hold a reference to itself. Posting never wakes anyone.
        let poster = Self {
            wakes: WakeManager::with_logger(server.db.clone(), logger),
            ..server.clone()
        };
        server.wakes.set_notifier(Arc::new(move |failure| {
            if let Err(err) = poster.post_wake_failure(failure) {
                poster.logger.log_error(
                    "wake.failure_notice_error",
                    format!("wake failure notice @{}: {err:#}", failure.principal_id),
                );
            }
        }));
        server
    }

    /// Tell the channel that woke an account that its agent did not run.
    /// The notice is not passed to webhooks or launchers, so a failing agent
    /// cannot set off more launches by being reported.
    fn post_wake_failure(&self, failure: &crate::wake::WakeFailure) -> Result<()> {
        let channel = failure.channel.as_str();
        let body = failure.notice();
        let message = self.history.append(channel, SERVER_NAME, &body)?;
        let mut tags = vec![
            ("seq", message.seq.to_string()),
            ("msg-id", message.msg_id.clone()),
            ("time", message.created_at.to_string()),
            ("wake-failed", failure.principal_id.clone()),
        ];
        if let Some(task_id) = &failure.task_id {
            tags.push(("task-id", task_id.clone()));
        }
        let tag_str = format_message_tags(&tags);
        if let Some((left, right)) = channel
            .strip_prefix("@dm:")
            .and_then(|pair| pair.split_once(':'))
        {
            // A direct message: tell the other side, who sent it.
            let peer = if left.eq_ignore_ascii_case(&failure.principal_id) {
                right
            } else {
                left
            };
            let client = self
                .inner
                .lock()
                .expect("server mutex poisoned")
                .clients
                .get(peer)
                .cloned();
            if let Some(client) = client {
                let target = self.display_name(peer).unwrap_or_else(|_| peer.to_string());
                let _ = client
                    .tx
                    .send(format!("{tag_str} :{SERVER_NAME} NOTICE {target} :{body}"));
            }
            return Ok(());
        }
        self.broadcast_message(
            channel,
            format!("{tag_str} :{SERVER_NAME} NOTICE {channel} :{body}"),
            message.seq,
        )
    }

    pub fn auth(&self) -> &AuthStore {
        &self.auth
    }

    pub fn logger(&self) -> &EventForwarder {
        &self.logger
    }

    pub fn backups(&self) -> &BackupManager {
        &self.backups
    }

    pub fn db(&self) -> &SharedConnection {
        &self.db
    }

    /// Commit the credential reset before invalidating the active IRC session.
    /// Registration and IRC command effects cannot straddle this operation.
    #[cfg(test)]
    pub fn reissue_token(&self, principal_id: &str) -> Result<String> {
        Ok(self.reissue_token_with_options(principal_id, None, None)?.token)
    }

    pub fn reissue_token_with_options(
        &self,
        principal_id: &str,
        scopes: Option<&[String]>,
        ttl_seconds: Option<u64>,
    ) -> Result<IssuedToken> {
        let _guard = self.session_gate.lock().expect("session mutex poisoned");
        let issued = self
            .auth
            .reissue_all_with_options(principal_id, scopes, ttl_seconds)?;
        let client = self
            .inner
            .lock()
            .expect("server mutex poisoned")
            .clients
            .remove(principal_id);
        if let Some(client) = client {
            let _ = client
                .shutdown_tx
                .send("Credentials reissued by owner".into());
        }
        self.logger.log_security(
            Severity::Info,
            "token.reissued",
            "success",
            Some("owner"),
            Some(principal_id),
            None,
            format!("Owner reissued token and revoked prior credentials for @{principal_id}"),
        );
        Ok(issued)
    }

    pub fn history(&self) -> &HistoryStore {
        &self.history
    }

    pub fn tasks(&self) -> &TaskStore {
        &self.tasks
    }

    pub fn webhooks(&self) -> &WebhookStore {
        &self.webhooks
    }

    pub fn reports(&self) -> &ReportStore {
        &self.reports
    }

    pub fn gits(&self) -> &GitRepoStore {
        &self.gits
    }

    pub fn rooms(&self) -> &RoomStore {
        &self.rooms
    }

    pub fn templates(&self) -> &ClientTemplateStore {
        &self.templates
    }

    pub fn groups(&self) -> &GroupStore {
        &self.groups
    }

    pub fn attachments(&self) -> &AttachmentStore {
        &self.attachments
    }

    /// Run once for deterministic tests, or from the background worker.
    pub fn reassign_inactive_group_tasks(&self, now: i64) -> Result<usize> {
        let _guard = self.task_gate.lock().expect("task mutex poisoned");
        let events = self.groups.reassign_inactive(now)?;
        for event in &events {
            let task = self.get_task_api(&event.task_id)?;
            self.broadcast_task_event(
                &task, "assign", "success", SERVER_NAME,
                format!("TASK {} automatically reassigned from @{} to @{} after inactivity in group {}: {}",
                    task.id, event.old_assignee, event.new_assignee, event.group, task.title),
            )?;
        }
        Ok(events.len())
    }

    pub fn spawn_group_inactivity_worker(&self) -> tokio::task::JoinHandle<()> {
        let server = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                let worker = server.clone();
                match tokio::task::spawn_blocking(move || {
                    worker.reassign_inactive_group_tasks(unix_timestamp())
                })
                .await
                {
                    Ok(Ok(_)) => {}
                    result => server.logger.log_error(
                        "group.inactivity_error",
                        format!("group inactivity sweep failed: {result:?}"),
                    ),
                }
            }
        })
    }

    pub fn notes(&self) -> &NoteStore {
        &self.notes
    }

    pub fn wakes(&self) -> &WakeManager {
        &self.wakes
    }

    pub fn projects(&self) -> &ProjectStore {
        &self.projects
    }

    pub fn locks(&self) -> &LockStore {
        &self.locks
    }

    pub async fn listen(self, bind_addr: &str) -> Result<()> {
        let listener = TcpListener::bind(bind_addr)
            .await
            .with_context(|| format!("bind {bind_addr}"))?;
        println!("aircd listening on {bind_addr}");

        loop {
            let (stream, peer_addr) = listener.accept().await?;
            let server = self.clone();
            tokio::spawn(async move {
                if let Err(error) = server.handle_stream(stream).await {
                    server.logger.log(
                        LogEvent::server_error(
                            "irc.connection_error",
                            format!("connection {peer_addr} closed: {error:#}"),
                        )
                        .with_target(peer_addr.to_string()),
                    );
                }
            });
        }
    }

    pub async fn listen_tls(self, bind_addr: &str, acceptor: TlsAcceptor) -> Result<()> {
        let listener = TcpListener::bind(bind_addr)
            .await
            .with_context(|| format!("bind {bind_addr}"))?;
        println!("aircd listening on {bind_addr} (TLS)");

        loop {
            let (stream, peer_addr) = listener.accept().await?;
            let acceptor = acceptor.clone();
            let server = self.clone();
            tokio::spawn(async move {
                let tls_stream = match acceptor.accept(stream).await {
                    Ok(stream) => stream,
                    Err(error) => {
                        server.logger.log(
                            LogEvent::server_error(
                                "irc.tls_handshake_failed",
                                format!("TLS handshake failed from {peer_addr}: {error:#}"),
                            )
                            .with_target(peer_addr.to_string()),
                        );
                        return;
                    }
                };
                if let Err(error) = server.handle_stream(tls_stream).await {
                    server.logger.log(
                        LogEvent::server_error(
                            "irc.tls_connection_error",
                            format!("connection {peer_addr} closed: {error:#}"),
                        )
                        .with_target(peer_addr.to_string()),
                    );
                }
            });
        }
    }

    async fn handle_stream<S>(&self, stream: S) -> Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (reader, mut writer) = tokio::io::split(stream);
        let mut lines = BufReader::new(reader).lines();
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let (shutdown_tx, mut shutdown_rx) = mpsc::unbounded_channel::<String>();

        let writer_task = tokio::spawn(async move {
            while let Some(line) = rx.recv().await {
                if writer.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
                if writer.write_all(b"\r\n").await.is_err() {
                    break;
                }
            }
        });

        let mut session = Session {
            session_id: format!("sess_{}", uuid::Uuid::new_v4().simple()),
            principal: None,
            token_info: None,
            nick: None,
            pass_token: None,
            registered: false,
            capabilities: HashSet::new(),
            tx,
            shutdown_tx,
        };

        session.send(format!(
            ":{SERVER_NAME} NOTICE * :Welcome to aircd prototype"
        ));

        let result: Result<()> = async {
            loop {
                tokio::select! {
                    biased;
                    shutdown_reason = shutdown_rx.recv() => {
                        if let Some(reason) = shutdown_reason {
                            session.send(format!(":{SERVER_NAME} ERROR :{reason}"));
                        }
                        break;
                    }
                    line = lines.next_line() => {
                        let Some(line) = line? else {
                            break;
                        };
                        let command = parse_command(&line);
                        let should_quit = self.handle_command(&mut session, command).await?;
                        if should_quit {
                            break;
                        }
                    }
                }
            }
            Ok(())
        }
        .await;

        let cleanup = self.disconnect(&session);
        writer_task.abort();
        result.and(cleanup)
    }

    async fn handle_command(&self, session: &mut Session, command: Command) -> Result<bool> {
        // Unregistered sessions acquire the gate in try_register instead.
        // There is no await while holding this synchronous guard.
        let _guard = session
            .registered
            .then(|| self.session_gate.lock().expect("session mutex poisoned"));
        if session.registered {
            self.ensure_registered(session)?;
        }
        match command {
            Command::Pass(token) => {
                session.pass_token = Some(token);
                self.try_register(session)?;
            }
            Command::Nick(nick) => {
                session.nick = Some(nick);
                self.try_register(session)?;
            }
            Command::User(_) => {
                self.try_register(session)?;
            }
            Command::Ping(token) => {
                session.send(format!(":{SERVER_NAME} PONG {SERVER_NAME} :{token}"));
            }
            Command::Pong => {}
            Command::Cap {
                subcommand,
                capabilities,
            } => {
                self.cap(session, subcommand, capabilities)?;
            }
            Command::Join(channels) => {
                if !self.require_irc_scope(session, Scope::Write, "JOIN")? {
                    return Ok(false);
                }
                for ch in channels.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                    self.join(session, normalize_channel(ch))?;
                }
            }
            Command::Part(channels) => {
                if !self.require_irc_scope(session, Scope::Write, "PART")? {
                    return Ok(false);
                }
                for ch in channels.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                    self.part(session, normalize_channel(ch))?;
                }
            }
            Command::Privmsg { target, body } => {
                if !self.require_irc_scope(session, Scope::Write, "PRIVMSG")? {
                    return Ok(false);
                }
                self.privmsg(session, target, body)?;
            }
            Command::History {
                channel,
                after_seq,
                limit,
            } => {
                if !self.require_irc_scope(session, Scope::Read, "HISTORY")? {
                    return Ok(false);
                }
                let channels = match channel {
                    Some(channel) => vec![normalize_channel(&channel)],
                    None => self
                        .memberships(session.principal()?.id.as_str())?
                        .into_iter()
                        .map(|membership| membership.channel)
                        .collect(),
                };
                self.replay_history(session, &channels, after_seq, limit)?;
            }
            Command::TaskCreate { channel, title } => {
                if !self.require_irc_scope(session, Scope::Pm, "TASK CREATE")? {
                    return Ok(false);
                }
                self.task_create(session, normalize_channel(&channel), title)?;
            }
            Command::TaskClaim(task_id) => {
                if !self.require_irc_scope(session, Scope::Write, "TASK CLAIM")? {
                    return Ok(false);
                }
                self.task_claim(session, task_id)?;
            }
            Command::TaskDone(task_id) => {
                if !self.require_irc_scope(session, Scope::Write, "TASK DONE")? {
                    return Ok(false);
                }
                self.task_done(session, task_id)?;
            }
            Command::TaskRelease(task_id) => {
                if !self.require_irc_scope(session, Scope::Write, "TASK RELEASE")? {
                    return Ok(false);
                }
                self.task_release(session, task_id)?;
            }
            Command::TaskList(channel) => {
                if !self.require_irc_scope(session, Scope::Read, "TASK LIST")? {
                    return Ok(false);
                }
                self.task_list(session, normalize_channel(&channel))?;
            }
            Command::Names(channel) => {
                if !self.require_irc_scope(session, Scope::Read, "NAMES")? {
                    return Ok(false);
                }
                self.names(session, channel)?;
            }
            Command::Who(target) => {
                if !self.require_irc_scope(session, Scope::Read, "WHO")? {
                    return Ok(false);
                }
                self.who(session, target)?;
            }
            Command::RoleList => {
                if !self.require_irc_scope(session, Scope::Read, "ROLES")? {
                    return Ok(false);
                }
                self.role_list(session)?;
            }
            Command::RoleSet { nick, role } => {
                if !self.require_irc_scope(session, Scope::Admin, "ROLE")? {
                    return Ok(false);
                }
                self.role_set(session, nick, role)?;
            }
            Command::LockAcquire {
                resource,
                channel,
                ttl_seconds,
                note,
            } => {
                if !self.require_irc_scope(session, Scope::Write, "LOCK")? {
                    return Ok(false);
                }
                self.lock_acquire(session, resource, channel, ttl_seconds, note)?;
            }
            Command::LockRelease(resource) => {
                if !self.require_irc_scope(session, Scope::Write, "UNLOCK")? {
                    return Ok(false);
                }
                self.lock_release(session, resource)?;
            }
            Command::LockList(channel) => {
                if !self.require_irc_scope(session, Scope::Read, "LOCKS")? {
                    return Ok(false);
                }
                self.lock_list(session, channel)?;
            }
            Command::Notice { target, body } => {
                if !self.require_irc_scope(session, Scope::Write, "NOTICE")? {
                    return Ok(false);
                }
                self.privmsg(session, target, body)?;
            }
            Command::Mode { target, flags, arg } => {
                let is_role_mutation = flags
                    .as_deref()
                    .is_some_and(|f| matches!(f, "+o" | "-o" | "+v" | "-v"))
                    && arg.is_some();
                let required = if is_role_mutation {
                    Scope::Admin
                } else {
                    Scope::Read
                };
                if !self.require_irc_scope(session, required, "MODE")? {
                    return Ok(false);
                }
                self.mode(session, target, flags, arg)?;
            }
            Command::Topic { channel, topic: _ } => {
                if !self.require_irc_scope(session, Scope::Read, "TOPIC")? {
                    return Ok(false);
                }
                self.send_topic(session, &normalize_channel(&channel))?;
            }
            Command::List(channel) | Command::Channels(channel) => {
                if !self.require_irc_scope(session, Scope::Read, "LIST")? {
                    return Ok(false);
                }
                self.list_channels(session, channel)?;
            }
            Command::Whois(nick) => {
                if !self.require_irc_scope(session, Scope::Read, "WHOIS")? {
                    return Ok(false);
                }
                self.whois(session, nick)?;
            }
            Command::Ison(nicks) => {
                if !self.require_irc_scope(session, Scope::Read, "ISON")? {
                    return Ok(false);
                }
                self.ison(session, nicks)?;
            }
            Command::Userhost(nicks) => {
                if !self.require_irc_scope(session, Scope::Read, "USERHOST")? {
                    return Ok(false);
                }
                self.userhost(session, nicks)?;
            }
            Command::Away(reason) => {
                if !self.require_irc_scope(session, Scope::Write, "AWAY")? {
                    return Ok(false);
                }
                let nick = session.principal()?.nick.clone();
                if reason.is_some() {
                    session.send(format!(
                        ":{SERVER_NAME} 306 {nick} :You have been marked as being away"
                    ));
                } else {
                    session.send(format!(
                        ":{SERVER_NAME} 305 {nick} :You are no longer marked as being away"
                    ));
                }
            }
            Command::Quit => return Ok(true),
            Command::Unknown(name) => {
                session.send(format!(":{SERVER_NAME} 421 * {name} :Unknown command"));
            }
        }

        Ok(false)
    }

    fn cap(
        &self,
        session: &mut Session,
        subcommand: String,
        requested_capabilities: Vec<String>,
    ) -> Result<()> {
        let nick = session.nick.as_deref().unwrap_or("*");
        let advertised = METADATA_CAPABILITIES.join(" ");

        match subcommand.as_str() {
            "LS" => {
                session.send(format!(":{SERVER_NAME} CAP {nick} LS :{advertised}"));
            }
            "LIST" => {
                let enabled = METADATA_CAPABILITIES
                    .iter()
                    .copied()
                    .filter(|cap| session.capabilities.contains(*cap))
                    .collect::<Vec<_>>()
                    .join(" ");
                session.send(format!(":{SERVER_NAME} CAP {nick} LIST :{enabled}"));
            }
            "REQ" => {
                let requested = requested_capabilities.join(" ");
                let all_supported = !requested_capabilities.is_empty()
                    && requested_capabilities
                        .iter()
                        .all(|capability| METADATA_CAPABILITIES.contains(&capability.as_str()));
                if all_supported {
                    for capability in requested_capabilities {
                        session.capabilities.insert(capability);
                    }
                }
                let status = if all_supported { "ACK" } else { "NAK" };
                session.send(format!(":{SERVER_NAME} CAP {nick} {status} :{requested}"));
            }
            "END" => {}
            _ => {
                session.send(format!(
                    ":{SERVER_NAME} 410 {nick} {} :Invalid CAP command",
                    subcommand
                ));
            }
        }

        Ok(())
    }

    fn try_register(&self, session: &mut Session) -> Result<()> {
        if session.registered {
            return Ok(());
        }
        let _guard = self.session_gate.lock().expect("session mutex poisoned");

        let Some(token) = session.pass_token.as_deref() else {
            return Ok(());
        };
        if session.nick.is_none() {
            return Ok(());
        }

        let ctx = match self.auth.authenticate_context(token) {
            Ok(ctx) => ctx,
            Err(_) => {
                self.logger.log_security(
                    Severity::Warning,
                    "auth.irc_failed",
                    "failure",
                    session.nick.as_deref(),
                    None,
                    Some("irc"),
                    "IRC authentication failed: invalid or expired token",
                );
                session.send(format!(":{SERVER_NAME} 464 * :Password incorrect"));
                return Ok(());
            }
        };
        let principal = ctx.principal;

        session.nick = Some(principal.nick.clone());
        session.principal = Some(principal.clone());
        session.token_info = Some(ctx.token);
        session.registered = true;

        let memberships = self.mark_connected_and_memberships(&principal.id)?;
        let channels = memberships
            .iter()
            .map(|membership| membership.channel.clone())
            .collect::<Vec<_>>();

        {
            let mut inner = self.inner.lock().expect("server mutex poisoned");
            if let Some(old_client) = inner.clients.insert(
                principal.id.clone(),
                ClientHandle {
                    session_id: session.session_id.clone(),
                    tx: session.tx.clone(),
                    shutdown_tx: session.shutdown_tx.clone(),
                },
            ) {
                let _ = old_client
                    .shutdown_tx
                    .send(format!("Replaced by a newer session for {}", principal.id));
                self.logger.log_security(
                    Severity::Info,
                    "auth.irc_session_replaced",
                    "success",
                    Some(&principal.id),
                    Some(&principal.id),
                    Some("irc"),
                    format!("Replaced active IRC session for @{}", principal.nick),
                );
            }

            for channel in &channels {
                inner
                    .channels
                    .entry(channel.clone())
                    .or_default()
                    .insert(principal.id.clone());
            }
        }

        session.send(format!(
            ":{SERVER_NAME} 001 {} :Welcome to aircd, {}{}",
            principal.nick,
            principal.nick,
            principal
                .team
                .as_ref()
                .map(|team| format!(" ({team})"))
                .unwrap_or_default()
        ));
        session.send(format!(
            ":{SERVER_NAME} 005 {} PREFIX=(ov)@+ CHANTYPES=#& CASEMAPPING=ascii NETWORK=aircd :are supported by this server",
            principal.nick
        ));
        session.send(format!(
            ":{SERVER_NAME} 375 {} :- aircd coordination server -",
            principal.nick
        ));
        session.send(format!(
            ":{SERVER_NAME} 372 {} :- Role: {} | Commands: JOIN, PRIVMSG <chan|nick>, NAMES, WHO, WHOIS, LIST, MODE, ROLE, ROLES, TASK, LOCK, UNLOCK, LOCKS",
            principal.nick, principal.role
        ));
        session.send(format!(
            ":{SERVER_NAME} 372 {} :- Webhooks, Reports, Git PR hooks & CLI onboarding: HTTP /v1/welcome",
            principal.nick
        ));
        session.send(format!(
            ":{SERVER_NAME} 376 {} :End of /MOTD command.",
            principal.nick
        ));

        for channel in &channels {
            session.send(format!(":{} JOIN {}", principal.nick, channel));
            let _ = self.send_topic(session, channel);
            let _ = self.send_channel_names(session, &principal.nick, channel);
        }

        if !memberships.is_empty() {
            session.send(format!(
                ":{SERVER_NAME} NOTICE {} :Replaying missed messages for {} channel(s)",
                principal.nick,
                memberships.len()
            ));
            for membership in memberships {
                self.replay_history(
                    session,
                    std::slice::from_ref(&membership.channel),
                    membership.last_seen_seq,
                    200,
                )?;
            }
        }

        Ok(())
    }

    fn join(&self, session: &Session, channel: String) -> Result<()> {
        let principal = session.principal()?;
        self.add_membership(&principal.id, &channel)?;

        let newly_joined_online = {
            let mut inner = self.inner.lock().expect("server mutex poisoned");
            inner
                .channels
                .entry(channel.clone())
                .or_default()
                .insert(principal.id.clone())
        };

        if newly_joined_online {
            self.broadcast_raw(&channel, format!(":{} JOIN {}", principal.nick, channel))?;
            let _ = self.send_topic(session, &channel);
            let _ = self.send_channel_names(session, &principal.nick, &channel);
        }
        Ok(())
    }

    fn part(&self, session: &Session, channel: String) -> Result<()> {
        let principal = session.principal()?;
        self.remove_membership(&principal.id, &channel)?;

        {
            let mut inner = self.inner.lock().expect("server mutex poisoned");
            if let Some(members) = inner.channels.get_mut(&channel) {
                members.remove(&principal.id);
            }
        }

        self.broadcast_raw(&channel, format!(":{} PART {}", principal.nick, channel))?;
        Ok(())
    }

    fn privmsg(&self, session: &Session, target: String, body: String) -> Result<()> {
        let principal = session.principal()?.clone();
        let trimmed_target = target.trim();
        if trimmed_target.starts_with('#') || trimmed_target.starts_with('&') {
            let channel = normalize_channel(trimmed_target);
            if !self.is_member(&principal.id, &channel)? {
                session.send(format!(
                    ":{SERVER_NAME} 442 {} {} :You're not on that channel",
                    principal.nick, channel
                ));
                return Ok(());
            }
            // Omit echoing back to the sender's IRC session unless echo-message was requested,
            // otherwise standard IRC clients render both local echo and server echo twice.
            let exclude_session = if session.capabilities.contains("echo-message") {
                None
            } else {
                Some(session.session_id.as_str())
            };
            self.deliver_channel_message(&principal, &channel, &body, exclude_session)?;
            return Ok(());
        }

        let peer_name = trimmed_target.trim_start_matches('@');
        if peer_name.is_empty() {
            session.send(format!(
                ":{SERVER_NAME} 411 {} :No recipient given (PRIVMSG)",
                principal.nick
            ));
            return Ok(());
        }
        let Some(peer) = self.auth.resolve_by_name(peer_name)? else {
            session.send(format!(
                ":{SERVER_NAME} 401 {} {} :No such nick",
                principal.nick, peer_name
            ));
            return Ok(());
        };
        self.deliver_direct_message(&principal, &peer, &body)?;
        Ok(())
    }

    fn replay_history(
        &self,
        session: &Session,
        channels: &[String],
        after_seq: i64,
        limit: i64,
    ) -> Result<()> {
        let principal = session.principal()?;

        for channel in channels {
            let mut channel_max_seq = after_seq;
            for message in self.history.after(channel, after_seq, limit)? {
                channel_max_seq = channel_max_seq.max(message.seq);
                let sender = self.display_name(&message.sender_id)?;
                let tags = metadata_tags(message.seq, &message.msg_id, message.created_at, true);
                session.send(format!(
                    "{tags} :{} PRIVMSG {} :{}",
                    sender, message.channel, message.body
                ));
            }

            if channel_max_seq > after_seq {
                self.mark_seen(&principal.id, channel, channel_max_seq)?;
            }
        }

        Ok(())
    }

    fn task_create(&self, session: &Session, channel: String, title: String) -> Result<()> {
        let principal = session.principal()?;
        if title.trim().is_empty() {
            session.send(format!(
                ":{SERVER_NAME} NOTICE {} :TASK CREATE failed: title is required",
                principal.nick
            ));
            return Ok(());
        }
        if channel.eq_ignore_ascii_case(CHAT_ONLY_CHANNEL) {
            session.send(format!(
                ":{SERVER_NAME} NOTICE {} :TASK CREATE failed: {CHAT_ONLY_CHANNEL} is chat only; create the task in a project channel",
                principal.nick
            ));
            return Ok(());
        }
        let task = self.tasks.create(&channel, title.trim())?;
        self.broadcast_task_event(
            &task,
            "create",
            "success",
            &principal.nick,
            format!(
                "TASK {} created by {}: {}",
                task.id, principal.nick, task.title
            ),
        )?;
        Ok(())
    }

    fn task_claim(&self, session: &Session, task_id: String) -> Result<()> {
        let principal = session.principal()?;
        let task = match self.tasks.claim(&task_id, &principal.id) {
            Ok(task) => task,
            Err(error) => {
                let body = format!("TASK CLAIM {} failed: {}", task_id, error);
                let tags = task_failure_tags("claim", &task_id, &principal.nick);
                session.send(format!(
                    "{tags} :{SERVER_NAME} NOTICE {} :{body}",
                    principal.nick
                ));
                return Ok(());
            }
        };
        self.broadcast_task_event(
            &task,
            "claim",
            "success",
            &principal.nick,
            format!(
                "TASK {} claimed by {}: {}",
                task.id, principal.nick, task.title
            ),
        )?;
        Ok(())
    }

    fn task_done(&self, session: &Session, task_id: String) -> Result<()> {
        let principal = session.principal()?;
        let task = match self.tasks.done(&task_id, &principal.id) {
            Ok(task) => task,
            Err(error) => {
                let body = format!("TASK DONE {} failed: {}", task_id, error);
                let tags = task_failure_tags("done", &task_id, &principal.nick);
                session.send(format!(
                    "{tags} :{SERVER_NAME} NOTICE {} :{body}",
                    principal.nick
                ));
                return Ok(());
            }
        };
        self.broadcast_task_event(
            &task,
            "done",
            "success",
            &principal.nick,
            format!(
                "TASK {} completed by {}: {}",
                task.id, principal.nick, task.title
            ),
        )?;
        Ok(())
    }

    fn task_release(&self, session: &Session, task_id: String) -> Result<()> {
        let principal = session.principal()?;
        let task = match self.tasks.release(&task_id, &principal.id) {
            Ok(task) => task,
            Err(error) => {
                let body = format!("TASK RELEASE {} failed: {}", task_id, error);
                let tags = task_failure_tags("release", &task_id, &principal.nick);
                session.send(format!(
                    "{tags} :{SERVER_NAME} NOTICE {} :{body}",
                    principal.nick
                ));
                return Ok(());
            }
        };
        self.broadcast_task_event(
            &task,
            "release",
            "success",
            &principal.nick,
            format!(
                "TASK {} released by {}: {}",
                task.id, principal.nick, task.title
            ),
        )?;
        Ok(())
    }

    fn task_list(&self, session: &Session, channel: String) -> Result<()> {
        let principal = session.principal()?;
        let tasks = self.tasks.list(&channel)?;
        if tasks.is_empty() {
            session.send(format!(
                ":{SERVER_NAME} NOTICE {} :No open tasks in {}",
                principal.nick, channel
            ));
            return Ok(());
        }

        for task in tasks {
            session.send(format!(
                ":{SERVER_NAME} NOTICE {} :TASK {} channel={} status={} claimed_by={} lease_expires_at={} title=:{}",
                principal.nick,
                task.id,
                task.channel,
                task.status,
                task.claimed_by.unwrap_or_else(|| "-".to_string()),
                task.lease_expires_at
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                task.title
            ));
        }

        Ok(())
    }

    fn broadcast_task_event(
        &self,
        task: &Task,
        action: &str,
        status: &str,
        actor: &str,
        mut body: String,
    ) -> Result<()> {
        let recipients = self.task_wake_recipients(task, action)?;
        let mut cc: Vec<_> = recipients
            .iter()
            .filter(|id| !id.eq_ignore_ascii_case(actor))
            .map(|id| format!("@{id}"))
            .collect();
        cc.sort();
        if !cc.is_empty() {
            body.push_str(&format!(" | cc {}", cc.join(" ")));
        }
        let room = task.sprint.clone().unwrap_or_else(|| task.channel.clone());
        let message = self.history.append(&room, SERVER_NAME, &body)?;
        let mut tags = vec![
            ("seq", message.seq.to_string()),
            ("msg-id", message.msg_id.clone()),
            ("time", message.created_at.to_string()),
            ("task-id", task.id.clone()),
            ("task-action", action.to_string()),
            ("task-status", status.to_string()),
            ("task-actor", actor.to_string()),
        ];
        if !task.title.is_empty() {
            tags.push(("task-title", task.title.clone()));
        }
        let tag_str = format_message_tags(
            &tags
                .iter()
                .map(|(k, v)| (*k, v.clone()))
                .collect::<Vec<_>>(),
        );
        self.broadcast_message(
            &room,
            format!("{tag_str} :{SERVER_NAME} NOTICE {room} :{body}"),
            message.seq,
        )?;
        let task_json = serde_json::to_value(task).ok();
        self.emit_webhook_to(
            WebhookEvent {
                event: "task".to_string(),
                channel: room,
                seq: Some(message.seq),
                msg_id: Some(message.msg_id),
                from: actor.to_string(),
                to: None,
                message: body,
                created_at: message.created_at,
                report: None,
                task: task_json,
                git: None,
                lock: None,
            },
            Some(&recipients),
        );
        Ok(())
    }

    /// Task wakes follow workflow ownership, never mentions in a title/report.
    pub(crate) fn task_wake_recipients(
        &self,
        task: &Task,
        action: &str,
    ) -> Result<HashSet<String>> {
        if let Some(round) = &task.review_round {
            if matches!(action, "status" | "peer_review")
                && matches!(task.status.as_str(), "review" | "super_review")
            {
                let ids = if task.status == "super_review" {
                    self.project_managers(&task.channel)?
                } else if action == "peer_review" {
                    // A rejected review needs the author's attention. Do not
                    // repeatedly wake peers after each partial approval.
                    if round.votes.iter().any(|v| v.decision == "reject") {
                        task.assignee.iter().cloned().collect()
                    } else {
                        Vec::new()
                    }
                } else {
                    self.eligible_review_peers(task, round)?
                };
                return Ok(ids.into_iter().collect());
            }
        }
        let reviewer = match task.reviewer.as_ref() {
            Some(id)
                if task.assignee.as_ref() != Some(id)
                    && self.projects.role_of(&task.channel, id)?.is_some() =>
            {
                Some(id)
            }
            _ => None,
        };
        let ids = match (action, task.status.as_str()) {
            ("create" | "assign" | "answer", _)
            | ("status", "changes_requested" | "merged" | "cancelled") => {
                task.assignee.iter().cloned().collect()
            }
            ("ask", _) | ("status", "review") => {
                if task.parent_task_id.is_some() {
                    match reviewer {
                        Some(id) => vec![id.clone()],
                        None => self.project_managers(&task.channel)?,
                    }
                } else {
                    let mut ids = self.project_managers(&task.channel)?;
                    ids.extend(reviewer.cloned());
                    ids
                }
            }
            ("escalate_pm", _) => match self.projects.get(&task.channel)?.and_then(|p| p.pm) {
                Some(pm) => vec![pm],
                None => self.project_managers(&task.channel)?,
            },
            ("escalate", _) => self.owner_ids()?,
            _ => Vec::new(),
        };
        Ok(ids.into_iter().map(|id| id.to_ascii_lowercase()).collect())
    }

    fn names(&self, session: &Session, channel: Option<String>) -> Result<()> {
        let principal = session.principal()?;
        let channels = match channel {
            Some(ch) => vec![normalize_channel(&ch)],
            None => self
                .memberships(&principal.id)?
                .into_iter()
                .map(|m| m.channel)
                .collect(),
        };

        for ch in channels {
            self.send_channel_names(session, &principal.nick, &ch)?;
        }
        Ok(())
    }

    fn send_channel_names(
        &self,
        session: &Session,
        caller_nick: &str,
        channel: &str,
    ) -> Result<()> {
        let entries = self.channel_member_directory(channel)?;
        let formatted = entries
            .into_iter()
            .map(|(_id, nick, role)| format!("{}{}", role_prefix(&role), nick))
            .collect::<Vec<_>>()
            .join(" ");
        session.send(format!(
            ":{SERVER_NAME} 353 {caller_nick} = {channel} :{formatted}"
        ));
        session.send(format!(
            ":{SERVER_NAME} 366 {caller_nick} {channel} :End of /NAMES list."
        ));
        Ok(())
    }

    fn send_topic(&self, session: &Session, channel: &str) -> Result<()> {
        let principal = session.principal()?;
        let locks = self.locks.list(Some(channel)).unwrap_or_default();
        let repos = self
            .gits
            .list()
            .unwrap_or_default()
            .into_iter()
            .filter(|r| r.channel.eq_ignore_ascii_case(channel))
            .map(|r| r.name)
            .collect::<Vec<_>>();
        if repos.is_empty() && locks.is_empty() {
            session.send(format!(
                ":{SERVER_NAME} 331 {} {} :No topic is set",
                principal.nick, channel
            ));
        } else {
            let mut parts = Vec::new();
            if !repos.is_empty() {
                parts.push(format!("Git repos: {}", repos.join(", ")));
            }
            if !locks.is_empty() {
                let lock_labels: Vec<String> = locks
                    .into_iter()
                    .map(|l| format!("{}(@{})", l.resource, l.owner_nick))
                    .collect();
                parts.push(format!("Locks: {}", lock_labels.join(", ")));
            }
            session.send(format!(
                ":{SERVER_NAME} 332 {} {} :{}",
                principal.nick,
                channel,
                parts.join(" | ")
            ));
        }
        Ok(())
    }

    fn mode(
        &self,
        session: &Session,
        target: String,
        flags: Option<String>,
        arg: Option<String>,
    ) -> Result<()> {
        let principal = session.principal()?.clone();
        let clean_target = target.trim();
        if !clean_target.starts_with('#') && !clean_target.starts_with('&') {
            session.send(format!(":{SERVER_NAME} 221 {} +i", principal.nick));
            return Ok(());
        }
        let channel = normalize_channel(clean_target);
        match (flags.as_deref(), arg.as_deref()) {
            (None, _) => {
                session.send(format!(
                    ":{SERVER_NAME} 324 {} {} +nt",
                    principal.nick, channel
                ));
            }
            (Some("b") | Some("+b"), None) => {
                session.send(format!(
                    ":{SERVER_NAME} 368 {} {} :End of Channel Ban List",
                    principal.nick, channel
                ));
            }
            (Some("+o"), Some(target_nick)) => {
                match self.set_role_api(&principal, target_nick, "leader", Some(&channel)) {
                    Ok(updated) => {
                        let _ = self.broadcast_raw(
                            &channel,
                            format!(":{} MODE {} +o {}", principal.nick, channel, updated.nick),
                        );
                    }
                    Err(err) => {
                        session.send(format!(
                            ":{SERVER_NAME} NOTICE {} :MODE {} +o {} failed: {}",
                            principal.nick, channel, target_nick, err
                        ));
                    }
                }
            }
            (Some("-o"), Some(target_nick)) => {
                match self.set_role_api(&principal, target_nick, "member", Some(&channel)) {
                    Ok(updated) => {
                        let _ = self.broadcast_raw(
                            &channel,
                            format!(":{} MODE {} -o {}", principal.nick, channel, updated.nick),
                        );
                    }
                    Err(err) => {
                        session.send(format!(
                            ":{SERVER_NAME} NOTICE {} :MODE {} -o {} failed: {}",
                            principal.nick, channel, target_nick, err
                        ));
                    }
                }
            }
            (Some("+v"), Some(target_nick)) => {
                match self.set_role_api(&principal, target_nick, "reviewer", Some(&channel)) {
                    Ok(updated) => {
                        let _ = self.broadcast_raw(
                            &channel,
                            format!(":{} MODE {} +v {}", principal.nick, channel, updated.nick),
                        );
                    }
                    Err(err) => {
                        session.send(format!(
                            ":{SERVER_NAME} NOTICE {} :MODE {} +v {} failed: {}",
                            principal.nick, channel, target_nick, err
                        ));
                    }
                }
            }
            (Some("-v"), Some(target_nick)) => {
                match self.set_role_api(&principal, target_nick, "member", Some(&channel)) {
                    Ok(updated) => {
                        let _ = self.broadcast_raw(
                            &channel,
                            format!(":{} MODE {} -v {}", principal.nick, channel, updated.nick),
                        );
                    }
                    Err(err) => {
                        session.send(format!(
                            ":{SERVER_NAME} NOTICE {} :MODE {} -v {} failed: {}",
                            principal.nick, channel, target_nick, err
                        ));
                    }
                }
            }
            _ => {
                session.send(format!(
                    ":{SERVER_NAME} 324 {} {} +nt",
                    principal.nick, channel
                ));
            }
        }
        Ok(())
    }

    fn list_channels(&self, session: &Session, filter: Option<String>) -> Result<()> {
        let principal = session.principal()?;
        session.send(format!(
            ":{SERVER_NAME} 321 {} Channel :Users  Name",
            principal.nick
        ));
        let filter_norm = filter.map(|f| normalize_channel(&f).to_ascii_lowercase());
        for (channel, msg_count, _) in self.history.recent_channels()? {
            if let Some(ref f) = filter_norm {
                if !channel.to_ascii_lowercase().contains(f) {
                    continue;
                }
            }
            let member_count = self.channel_member_ids(&channel)?.len();
            session.send(format!(
                ":{SERVER_NAME} 322 {} {} {} :messages={}",
                principal.nick, channel, member_count, msg_count
            ));
        }
        session.send(format!(
            ":{SERVER_NAME} 323 {} :End of /LIST",
            principal.nick
        ));
        Ok(())
    }

    fn whois(&self, session: &Session, target_nick: String) -> Result<()> {
        let caller = session.principal()?;
        let clean = target_nick.trim().trim_start_matches('@');
        let Some(target) = self.auth.resolve_by_name(clean)? else {
            session.send(format!(
                ":{SERVER_NAME} 401 {} {} :No such nick",
                caller.nick, clean
            ));
            session.send(format!(
                ":{SERVER_NAME} 318 {} {} :End of /WHOIS list.",
                caller.nick, clean
            ));
            return Ok(());
        };

        session.send(format!(
            ":{SERVER_NAME} 311 {} {} {} aircd * :role={} team={}",
            caller.nick,
            target.nick,
            target.id,
            target.role,
            target.team.as_deref().unwrap_or("-")
        ));
        let joined: Vec<String> = self
            .memberships(&target.id)?
            .into_iter()
            .map(|m| m.channel)
            .collect();
        if !joined.is_empty() {
            session.send(format!(
                ":{SERVER_NAME} 319 {} {} :{}",
                caller.nick,
                target.nick,
                joined.join(" ")
            ));
        }
        session.send(format!(
            ":{SERVER_NAME} 312 {} {} aircd :aircd coordination server",
            caller.nick, target.nick
        ));
        session.send(format!(
            ":{SERVER_NAME} 318 {} {} :End of /WHOIS list.",
            caller.nick, target.nick
        ));
        Ok(())
    }

    fn ison(&self, session: &Session, nicks: Vec<String>) -> Result<()> {
        let caller = session.principal()?;
        let online = self.online_principal_ids();
        let mut present = Vec::new();
        for nick in nicks {
            if let Some(p) = self.auth.resolve_by_name(&nick)? {
                if online.contains(&p.id) {
                    present.push(p.nick);
                }
            }
        }
        session.send(format!(
            ":{SERVER_NAME} 303 {} :{}",
            caller.nick,
            present.join(" ")
        ));
        Ok(())
    }

    fn userhost(&self, session: &Session, nicks: Vec<String>) -> Result<()> {
        let caller = session.principal()?;
        let mut entries = Vec::new();
        for nick in nicks.into_iter().take(5) {
            if let Some(p) = self.auth.resolve_by_name(&nick)? {
                entries.push(format!("{}=+{}@aircd", p.nick, p.id));
            }
        }
        session.send(format!(
            ":{SERVER_NAME} 302 {} :{}",
            caller.nick,
            entries.join(" ")
        ));
        Ok(())
    }

    fn who(&self, session: &Session, target: Option<String>) -> Result<()> {
        let principal = session.principal()?;
        let online = self.online_principal_ids();
        let target_label = target.clone().unwrap_or_else(|| "*".to_string());

        match target {
            Some(ref ch) if ch.starts_with('#') || ch.starts_with('&') => {
                let channel = normalize_channel(ch);
                for (id, nick, role) in self.channel_member_directory(&channel)? {
                    let presence = if online.contains(&id) { "H" } else { "G" };
                    let pfx = role_prefix(&role);
                    session.send(format!(
                        ":{SERVER_NAME} 352 {} {} {} aircd aircd {} {}{} :0 role={}",
                        principal.nick, channel, id, nick, presence, pfx, role
                    ));
                }
            }
            _ => {
                let query = target.as_deref().map(|t| t.trim_start_matches('@'));
                for p in self.auth.list_principals()? {
                    if let Some(q) = query {
                        if q != "*"
                            && !p.id.eq_ignore_ascii_case(q)
                            && !p.nick.eq_ignore_ascii_case(q)
                        {
                            continue;
                        }
                    }
                    let presence = if online.contains(&p.id) { "H" } else { "G" };
                    let pfx = role_prefix(&p.role);
                    session.send(format!(
                        ":{SERVER_NAME} 352 {} * {} aircd aircd {} {}{} :0 role={}",
                        principal.nick, p.id, p.nick, presence, pfx, p.role
                    ));
                }
            }
        }

        session.send(format!(
            ":{SERVER_NAME} 315 {} {} :End of /WHO list.",
            principal.nick, target_label
        ));
        Ok(())
    }

    fn role_list(&self, session: &Session) -> Result<()> {
        let principal = session.principal()?;
        let online = self.online_principal_ids();
        for p in self.auth.list_principals()? {
            let status = if online.contains(&p.id) {
                "online"
            } else {
                "offline"
            };
            session.send(format!(
                ":{SERVER_NAME} NOTICE {} :ROLE @{} role={} team={} status={}",
                principal.nick,
                p.nick,
                p.role,
                p.team.as_deref().unwrap_or("-"),
                status
            ));
        }
        Ok(())
    }

    fn role_set(&self, session: &Session, target_nick: String, role: String) -> Result<()> {
        let principal = session.principal()?.clone();
        match self.set_role_api(&principal, &target_nick, &role, None) {
            Ok(_) => Ok(()),
            Err(err) => {
                session.send(format!(
                    ":{SERVER_NAME} NOTICE {} :ROLE {} {} failed: {}",
                    principal.nick, target_nick, role, err
                ));
                Ok(())
            }
        }
    }

    fn lock_acquire(
        &self,
        session: &Session,
        resource: String,
        channel: Option<String>,
        ttl_seconds: Option<i64>,
        note: Option<String>,
    ) -> Result<()> {
        let principal = session.principal()?.clone();
        let target_channel = match channel {
            Some(c) => normalize_channel(&c),
            None => self
                .memberships(&principal.id)?
                .into_iter()
                .next()
                .map(|m| m.channel)
                .unwrap_or_else(|| "#general".to_string()),
        };

        if let Err(err) = self.acquire_lock_api(
            &principal,
            &resource,
            &target_channel,
            note.as_deref(),
            ttl_seconds,
        ) {
            session.send(format!(
                ":{SERVER_NAME} NOTICE {} :LOCK {} failed: {}",
                principal.nick, resource, err
            ));
        }
        Ok(())
    }

    fn lock_release(&self, session: &Session, resource: String) -> Result<()> {
        let principal = session.principal()?.clone();
        let is_privileged_role = matches!(
            principal.role.as_str(),
            "leader" | "coordinator" | "op" | "admin"
        );
        let can_force = is_privileged_role && session.has_scope(Scope::Admin);
        if let Err(err) = self.release_lock_api(&principal, &resource, can_force) {
            if err.downcast_ref::<LockConflictError>().is_some()
                && is_privileged_role
                && !session.has_scope(Scope::Admin)
            {
                self.logger.log_security(
                    Severity::Warning,
                    "auth.scope_denied",
                    "failure",
                    Some(&principal.id),
                    None,
                    Some("irc"),
                    format!(
                        "IRC UNLOCK force-unlock denied for @{}: token lacks required scope 'admin'",
                        principal.nick
                    ),
                );
                session.send(format!(
                    ":{SERVER_NAME} 481 {} :Permission denied - UNLOCK (force) requires token scope 'admin'",
                    principal.nick
                ));
                return Ok(());
            }
            session.send(format!(
                ":{SERVER_NAME} NOTICE {} :UNLOCK {} failed: {}",
                principal.nick, resource, err
            ));
        }
        Ok(())
    }

    fn lock_list(&self, session: &Session, channel: Option<String>) -> Result<()> {
        let principal = session.principal()?;
        let chan = channel.map(|c| normalize_channel(&c));
        let locks = self.locks.list(chan.as_deref())?;
        if locks.is_empty() {
            session.send(format!(
                ":{SERVER_NAME} NOTICE {} :No active equipment or branch locks{}",
                principal.nick,
                chan.as_deref()
                    .map(|c| format!(" in {c}"))
                    .unwrap_or_default()
            ));
            return Ok(());
        }
        for lock in locks {
            session.send(format!(
                ":{SERVER_NAME} NOTICE {} :LOCK {} holder=@{} channel={} expires_at={} note=:{}",
                principal.nick,
                lock.resource,
                lock.owner_nick,
                lock.channel,
                lock.expires_at,
                lock.note.as_deref().unwrap_or("-")
            ));
        }
        Ok(())
    }

    pub fn join_channels(
        &self,
        principal: &Principal,
        raw_channels: &[String],
    ) -> Result<Vec<String>> {
        let mut joined = Vec::new();
        for raw in raw_channels {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                continue;
            }
            let channel = normalize_channel(trimmed);
            self.add_membership(&principal.id, &channel)?;
            let newly_joined_online = {
                let mut inner = self.inner.lock().expect("server mutex poisoned");
                if inner.clients.contains_key(&principal.id) {
                    inner
                        .channels
                        .entry(channel.clone())
                        .or_default()
                        .insert(principal.id.clone())
                } else {
                    false
                }
            };
            if newly_joined_online {
                let _ =
                    self.broadcast_raw(&channel, format!(":{} JOIN {}", principal.nick, channel));
            }
            if !joined.contains(&channel) {
                joined.push(channel);
            }
        }
        Ok(joined)
    }

    pub fn post_message(
        &self,
        sender: &Principal,
        raw_target: &str,
        raw_body: &str,
    ) -> Result<StoredMessage> {
        let body = raw_body.trim_end_matches(['\r', '\n']);
        if body.trim().is_empty() {
            return Err(anyhow!("message body is required"));
        }
        let target = raw_target.trim();
        if target.is_empty() {
            return Err(anyhow!("target channel or @nick is required"));
        }
        if target.starts_with('#') || target.starts_with('&') {
            let channel = normalize_channel(target);
            self.join_channels(sender, std::slice::from_ref(&channel))?;
            self.deliver_channel_message(sender, &channel, body, None)
        } else {
            let peer_name = target.trim_start_matches('@');
            let peer = self
                .auth
                .resolve_by_name(peer_name)?
                .ok_or_else(|| anyhow!("Unknown recipient '@{peer_name}'"))?;
            self.deliver_direct_message(sender, &peer, body)
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn post_report(
        &self,
        author: &Principal,
        raw_channel: &str,
        title: &str,
        status: &str,
        summary: Option<&str>,
        body: &str,
        task_id: Option<&str>,
        git_repo: Option<&str>,
        git_branch: Option<&str>,
        git_commit: Option<&str>,
    ) -> Result<Report> {
        let channel = normalize_channel(raw_channel);
        self.join_channels(author, std::slice::from_ref(&channel))?;
        let report = self.reports.create(NewReport {
            author_id: &author.id,
            channel: &channel,
            title,
            status,
            summary,
            body,
            task_id,
            git_repo,
            git_branch,
            git_commit,
        })?;

        let notice_body = format!(
            "REPORT {} [{}] by @{}: {} — {}",
            report.id, report.status, report.author_nick, report.title, report.summary
        );
        let message = self.history.append(&channel, &author.id, &notice_body)?;
        let _ = self.mark_seen(&author.id, &channel, message.seq);

        let tag_str = format_message_tags(&[
            ("seq", message.seq.to_string()),
            ("msg-id", message.msg_id.clone()),
            ("time", message.created_at.to_string()),
            ("report-id", report.id.clone()),
            ("report-status", report.status.clone()),
        ]);
        self.broadcast_message(
            &channel,
            format!("{tag_str} :{SERVER_NAME} NOTICE {channel} :{notice_body}"),
            message.seq,
        )?;

        self.emit_webhook(WebhookEvent {
            event: "report".to_string(),
            channel,
            seq: Some(message.seq),
            msg_id: Some(message.msg_id),
            from: author.id.clone(),
            to: None,
            message: notice_body,
            created_at: message.created_at,
            report: serde_json::to_value(&report).ok(),
            task: None,
            git: None,
            lock: None,
        });

        Ok(report)
    }

    // ---- projects ------------------------------------------------------

    fn require_project(&self, channel: &str) -> Result<Project> {
        let project = self.project_of(channel);
        let channel = project.as_str();
        self.projects.get(channel)?.ok_or_else(|| {
            anyhow!("{channel} is not a project; create it with POST /v1/projects first")
        })
    }

    /// Project leaders and coordinators manage a project's work. A global
    /// leader, op or admin (the owner) manages every project.
    pub fn is_project_manager(&self, actor: &Principal, channel: &str) -> Result<bool> {
        let project = self.project_of(channel);
        let channel = project.as_str();
        if matches!(actor.role.as_str(), "leader" | "op" | "admin") {
            return Ok(true);
        }
        Ok(self
            .projects
            .role_of(channel, &actor.id)?
            .is_some_and(|role| MANAGER_ROLES.contains(&role.as_str())))
    }

    /// The project manager first, then the other leaders and coordinators.
    fn project_managers(&self, channel: &str) -> Result<Vec<String>> {
        let project = self.project_of(channel);
        let channel = project.as_str();
        let mut out: Vec<String> = self
            .projects
            .get(channel)?
            .and_then(|project| project.pm)
            .into_iter()
            .collect();
        for member in self.projects.members(channel)? {
            if MANAGER_ROLES.contains(&member.role.as_str()) && !out.contains(&member.principal_id)
            {
                out.push(member.principal_id);
            }
        }
        Ok(out)
    }

    /// The owner: a global leader, op or admin.
    pub fn is_owner(actor: &Principal) -> bool {
        matches!(actor.role.as_str(), "leader" | "op" | "admin")
    }

    fn owner_ids(&self) -> Result<Vec<String>> {
        Ok(self
            .auth
            .list_principals()?
            .into_iter()
            .filter(Self::is_owner)
            .map(|p| p.id)
            .collect())
    }

    /// Post a server notice in a channel and notify whoever it mentions.
    fn announce(&self, channel: &str, actor: &Principal, body: String) -> Result<()> {
        let message = self.history.append(channel, &actor.id, &body)?;
        let tag_str = format_message_tags(&[
            ("seq", message.seq.to_string()),
            ("msg-id", message.msg_id.clone()),
            ("time", message.created_at.to_string()),
        ]);
        self.broadcast_message(
            channel,
            format!("{tag_str} :{SERVER_NAME} NOTICE {channel} :{body}"),
            message.seq,
        )?;
        self.emit_webhook(WebhookEvent {
            event: "role".to_string(),
            channel: channel.to_string(),
            seq: Some(message.seq),
            msg_id: Some(message.msg_id),
            from: actor.id.clone(),
            to: None,
            message: body,
            created_at: message.created_at,
            report: None,
            task: None,
            git: None,
            lock: None,
        });
        Ok(())
    }

    /// Owner only: choose the one project manager of a project. The PM gets
    /// the coordinator role; a replaced PM steps back to member.
    pub fn set_project_pm_api(
        &self,
        actor: &Principal,
        raw_channel: &str,
        target_name: &str,
    ) -> Result<Project> {
        if !Self::is_owner(actor) {
            return Err(anyhow!("only the owner can choose a project manager"));
        }
        let channel = normalize_channel(raw_channel);
        let project = self.require_project(&channel)?;
        let target = self
            .auth
            .resolve_by_name(target_name.trim().trim_start_matches('@'))?
            .ok_or_else(|| anyhow!("unknown account '{target_name}'"))?;

        if let Some(previous) = project.pm.as_deref() {
            if previous != target.id
                && self.projects.role_of(&channel, previous)?.as_deref() == Some("coordinator")
            {
                self.projects.set_role(&channel, previous, "member")?;
            }
        }
        self.projects.set_pm(&channel, Some(&target.id))?;
        self.projects.set_role(&channel, &target.id, "coordinator")?;
        self.join_channels(&target, std::slice::from_ref(&channel))?;
        self.join_rooms(&target, &channel)?;
        self.join_channels(actor, std::slice::from_ref(&channel))?;
        self.announce(
            &channel,
            actor,
            format!(
                "PM of {channel} is now @{} (set by @{})",
                target.nick, actor.nick
            ),
        )?;
        self.logger.log_security(
            Severity::Info,
            "project.pm_changed",
            "success",
            Some(&actor.id),
            Some(&target.id),
            Some(&channel),
            format!("PM of {channel} set to @{} by @{}", target.nick, actor.nick),
        );
        self.require_project(&channel)
    }

    // ---- sprint rooms --------------------------------------------------

    /// The project a channel belongs to: a sprint room maps to its project,
    /// anything else is returned unchanged.
    pub fn project_of(&self, channel: &str) -> String {
        self.rooms
            .get(channel)
            .ok()
            .flatten()
            .map(|room| room.project)
            .unwrap_or_else(|| channel.to_string())
    }

    /// Everyone on a project is in every one of its rooms.
    fn join_rooms(&self, principal: &Principal, project: &str) -> Result<()> {
        let rooms: Vec<String> = self
            .rooms
            .list(Some(project))?
            .into_iter()
            .map(|room| room.channel)
            .collect();
        if !rooms.is_empty() {
            self.join_channels(principal, &rooms)?;
        }
        Ok(())
    }

    /// Open the project's next sprint room. Rooms are never closed; the
    /// newest one is the current sprint, where new tasks go by default.
    pub fn open_room_api(
        &self,
        actor: &Principal,
        raw_project: &str,
        title: Option<&str>,
    ) -> Result<Room> {
        let project = self.project_of(&normalize_channel(raw_project));
        self.require_project(&project)?;
        if !self.is_project_manager(actor, &project)? {
            return Err(anyhow!(
                "only the owner or a leader or coordinator of {project} can open a sprint"
            ));
        }
        let room = self.rooms.open(&project, title, &actor.id)?;

        let mut people = vec![actor.clone()];
        for member in self.projects.members(&project)? {
            if let Some(principal) = self.auth.resolve_by_name(&member.principal_id)? {
                people.push(principal);
            }
        }
        for person in &people {
            self.join_channels(person, std::slice::from_ref(&room.channel))?;
        }

        let name = room
            .title
            .as_deref()
            .map(|title| format!(": {title}"))
            .unwrap_or_default();
        self.announce(
            &project,
            actor,
            format!(
                "Sprint {} opened by {}{name} - room {}",
                room.label, actor.nick, room.channel
            ),
        )?;
        self.announce(
            &room.channel,
            actor,
            format!(
                "Sprint {} of {project}{name}. New tasks go here; this room is the record of the sprint.",
                room.label
            ),
        )?;
        Ok(room)
    }

    // ---- questions -----------------------------------------------------

    /// The assignee cannot continue without a decision: park the task and
    /// ask the project manager.
    pub fn ask_task_api(&self, actor: &Principal, task_id: &str, question: &str) -> Result<Task> {
        let question = question.trim();
        if question.is_empty() {
            return Err(anyhow!("a question is required"));
        }
        let current = self.get_task_api(task_id)?;
        if current.assignee.as_deref() != Some(actor.id.as_str()) {
            return Err(anyhow!("only the task's assignee can ask a question on it"));
        }
        let (task, _) = self.tasks.ask(&current.id, &actor.id, question)?;
        let text = format!(
            "TASK {} blocked - @{} asks: {}",
            task.id, actor.nick, question
        );
        self.broadcast_task_event(&task, "ask", "success", &actor.nick, text)?;
        Ok(task)
    }

    /// A project manager, or the owner, answers the pending question; the
    /// task returns to where it was and the asker is notified.
    pub fn answer_task_api(&self, actor: &Principal, task_id: &str, answer: &str) -> Result<Task> {
        let answer = answer.trim();
        if answer.is_empty() {
            return Err(anyhow!("an answer is required"));
        }
        let current = self.get_task_api(task_id)?;
        let is_manager = self.is_project_manager(actor, &current.channel)?;
        let can_review = self.is_task_reviewer(actor, &current)?;
        let is_open = self
            .tasks
            .pending_question(&current.id)?
            .is_some_and(|q| q.status == "open");
        if !(is_manager || can_review && is_open) {
            return Err(anyhow!(
                "only the subtask reviewer, a leader or coordinator of {} or the owner can answer",
                current.channel
            ));
        }
        let (task, question) = self.tasks.answer(&current.id, &actor.id, answer)?;
        let text = format!(
            "TASK {} answered by @{} for @{}: {}",
            task.id, actor.nick, question.asked_by, answer
        );
        self.broadcast_task_event(&task, "answer", "success", &actor.nick, text)?;
        Ok(task)
    }

    /// A project manager passes a question it cannot decide up to the owner.
    pub fn escalate_task_api(
        &self,
        actor: &Principal,
        task_id: &str,
        note: Option<&str>,
    ) -> Result<Task> {
        let current = self.get_task_api(task_id)?;
        let is_manager = self.is_project_manager(actor, &current.channel)?;
        if !is_manager && !self.is_task_reviewer(actor, &current)? {
            return Err(anyhow!(
                "only the subtask reviewer or a leader or coordinator of {} can escalate",
                current.channel
            ));
        }
        let note = note.map(str::trim).filter(|n| !n.is_empty());
        let question = if is_manager {
            self.tasks.escalate(&current.id, &actor.id, note)?
        } else {
            self.tasks.escalate_to_pm(&current.id, &actor.id, note)?
        };
        let destination = if is_manager {
            "the owner"
        } else {
            "the project manager"
        };
        let mut text = format!(
            "TASK {} needs {} - @{} escalates @{}'s question: {}",
            current.id, destination, actor.nick, question.asked_by, question.question
        );
        if let Some(note) = note {
            text.push_str(&format!(" ({note})"));
        }
        self.broadcast_task_event(
            &current,
            if is_manager {
                "escalate"
            } else {
                "escalate_pm"
            },
            "success",
            &actor.nick,
            text,
        )?;
        Ok(current)
    }

    fn is_task_reviewer(&self, actor: &Principal, task: &Task) -> Result<bool> {
        Ok(task.parent_task_id.is_some()
            && task.assignee.as_deref() != Some(&actor.id)
            && task.reviewer.as_deref() == Some(&actor.id)
            && self.projects.role_of(&task.channel, &actor.id)?.is_some())
    }

    pub fn create_project_api(
        &self,
        actor: &Principal,
        raw_channel: &str,
        name: &str,
        description: Option<&str>,
    ) -> Result<Project> {
        if !matches!(
            actor.role.as_str(),
            "leader" | "coordinator" | "op" | "admin"
        ) {
            return Err(anyhow!(
                "only a leader or coordinator can create a project"
            ));
        }
        let channel = normalize_channel(raw_channel);
        let project = self
            .projects
            .create(&channel, name, description, &actor.id)?;
        self.projects.set_role(&channel, &actor.id, "leader")?;
        self.join_channels(actor, std::slice::from_ref(&channel))?;
        Ok(project)
    }

    /// Add an account to a project (joining its channel) or change its role.
    pub fn set_project_member_api(
        &self,
        actor: &Principal,
        raw_channel: &str,
        target_name: &str,
        raw_role: Option<&str>,
    ) -> Result<ProjectMember> {
        let channel = normalize_channel(raw_channel);
        self.require_project(&channel)?;
        if !self.is_project_manager(actor, &channel)? {
            return Err(anyhow!(
                "only a leader or coordinator of {channel} can change its members"
            ));
        }
        let target = self
            .auth
            .resolve_by_name(target_name.trim().trim_start_matches('@'))?
            .ok_or_else(|| anyhow!("unknown account '{target_name}'"))?;
        let role = crate::auth::normalize_role(raw_role.unwrap_or("member"))?;
        self.projects.set_role(&channel, &target.id, &role)?;
        self.join_channels(&target, std::slice::from_ref(&channel))?;
        self.join_rooms(&target, &channel)?;
        self.logger.log_security(
            Severity::Info,
            "project.member_changed",
            "success",
            Some(&actor.id),
            Some(&target.id),
            Some(&channel),
            format!(
                "Project member @{} role in {channel} set to '{}' by @{}",
                target.nick, role, actor.nick
            ),
        );
        Ok(ProjectMember {
            principal_id: target.id,
            role,
        })
    }

    /// Owner action: create an account that runs `model` through a client
    /// template, give it the template's launcher with the model filled in and
    /// add it to a project. Everything is checked before anything is written.
    pub fn create_account_from_template(
        &self,
        template_id: &str,
        raw_model: &str,
        raw_nick: Option<&str>,
        raw_channel: &str,
        raw_role: Option<&str>,
    ) -> Result<CreatedAccount> {
        let template = self
            .templates
            .get(template_id)?
            .ok_or_else(|| anyhow!("no client template named '{}'", template_id.trim()))?;
        let model = raw_model.trim();
        if !crate::store::valid_model(model) {
            return Err(anyhow!("a model is required: one word without braces"));
        }
        let channel = self.project_of(&normalize_channel(raw_channel));
        self.require_project(&channel)?;
        let role = crate::auth::normalize_role(raw_role.unwrap_or("member"))?;
        let nick = crate::auth::validate_nick(
            raw_nick.map(str::trim).filter(|n| !n.is_empty()).unwrap_or(model),
        )?;
        if self.auth.resolve_by_name(&nick)?.is_some() {
            return Err(anyhow!("an account named '{nick}' already exists"));
        }
        self.provision_account(template, model, &nick, &channel, &[channel.clone()], &role)
    }

    /// Spawn from an owner-defined template. `None` is reserved for an HTTP
    /// caller that has already supplied the owner key. Bearer callers must
    /// own the hub or manage every requested project; roles remain local to
    /// those projects and the global account is always a member.
    pub(crate) fn spawn_worker_from_template(
        &self,
        actor: Option<&Principal>,
        template_id: &str,
        raw_model: &str,
        raw_nick: &str,
        raw_channels: &[String],
        raw_role: Option<&str>,
    ) -> Result<CreatedAccount> {
        let mut channels = Vec::new();
        for raw in raw_channels {
            let channel = normalize_channel(raw);
            if raw.trim().is_empty()
                || channel.len() < 2
                || channel.chars().any(|c| {
                    c.is_whitespace() || c.is_control() || matches!(c, ',' | ':' | '\u{7}')
                })
            {
                return Err(anyhow!(
                    "channels must be nonempty channel names without spaces, commas or colons"
                ));
            }
            if !channels.contains(&channel) {
                channels.push(channel);
            }
        }
        if channels.is_empty() {
            channels.push(CHAT_ONLY_CHANNEL.to_string());
        }
        if let Some(actor) = actor.filter(|actor| !Self::is_owner(actor)) {
            for channel in &channels {
                let project = self.project_of(channel);
                if self.projects.get(&project)?.is_none()
                    || !self.is_project_manager(actor, &project)?
                {
                    return Err(SpawnError::Forbidden(format!(
                        "only the owner or a manager of {channel} can provision a worker there"
                    ))
                    .into());
                }
            }
        }
        let template = self.templates.get(template_id)?.ok_or_else(|| {
            SpawnError::TemplateNotFound(format!("no client template named '{template_id}'"))
        })?;
        let model = raw_model.trim();
        if !crate::store::valid_model(model) {
            return Err(anyhow!("a model is required: one word without braces"));
        }
        let nick = crate::auth::validate_nick(raw_nick)?;
        let role = crate::auth::normalize_role(raw_role.unwrap_or("member"))?;
        self.provision_account(template, model, &nick, &channels[0], &channels, &role)
    }

    fn provision_account(
        &self,
        template: ClientTemplate,
        model: &str,
        nick: &str,
        channel: &str,
        requested_channels: &[String],
        role: &str,
    ) -> Result<CreatedAccount> {
        let mut channels = vec![CHAT_ONLY_CHANNEL.to_string()];
        let mut projects = Vec::new();
        for channel in requested_channels {
            if !channels.contains(channel) {
                channels.push(channel.clone());
            }
            let project = self.project_of(channel);
            if self.projects.get(&project)?.is_some() && !projects.contains(&project) {
                if !channels.contains(&project) {
                    channels.push(project.clone());
                }
                for room in self.rooms.list(Some(&project))? {
                    if !channels.contains(&room.channel) {
                        channels.push(room.channel);
                    }
                }
                projects.push(project);
            }
        }

        // A failed launcher/membership write must not strand an account whose
        // only token was never returned to the caller. Nothing launches here.
        let mut db = self.db.write()?;
        let tx = db.transaction().context("start worker provisioning")?;
        let (principal, issued) = AuthStore::register_principal_on_with_options(
            &tx,
            nick,
            None,
            None,
            None,
            self.auth.default_ttl_seconds(),
        )?;
        let token = issued.token;
        let wake = WakeManager::upsert_on(&tx, &principal.id, template.launcher_for(model))?;
        for project in projects {
            tx.execute(
                "INSERT INTO project_roles (channel, principal_id, role, updated_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![project, principal.id, role, unix_timestamp()],
            )?;
        }
        for channel in &channels {
            tx.execute(
                "INSERT INTO channel_memberships (principal_id, channel, joined_at, last_seen_seq)
                 VALUES (?1, ?2, ?3, (SELECT COALESCE(MAX(seq), 0) FROM messages WHERE channel = ?2))",
                params![principal.id, channel, unix_timestamp()],
            )?;
        }
        tx.commit().context("commit worker provisioning")?;
        Ok(CreatedAccount {
            principal,
            token,
            channel: channel.to_string(),
            channels,
            role: role.to_string(),
            model: model.to_string(),
            template: template.id,
            wake,
        })
    }

    // ---- tasks ---------------------------------------------------------

    /// Resolve who a task may be given to. Assignment only exists inside a
    /// project, and only to an account that is a member of that project.
    fn resolve_assignee(
        &self,
        actor: &Principal,
        channel: &str,
        name: &str,
        parent: Option<&Task>,
    ) -> Result<String> {
        let target_id = self.resolve_task_account(channel, name)?;
        let is_lead = parent.is_some_and(|task| task.assignee.as_deref() == Some(&actor.id));
        let is_manager = self.is_project_manager(actor, channel)?;
        if let Some(parent) = parent.filter(|_| !is_manager) {
            let parent_lead = parent
                .assignee
                .as_deref()
                .map(|id| self.auth.resolve_by_name(id))
                .transpose()?
                .flatten();
            if let Some(binding) = parent_lead
                .as_ref()
                .map(|lead| {
                    self.groups
                        .named_role(&self.project_of(channel), &lead.nick)
                })
                .transpose()?
                .flatten()
            {
                if !binding.is_lead {
                    if target_id != actor.id {
                        return Err(anyhow!(
                            "only a group lead can delegate to other group members"
                        ));
                    }
                } else if parent.assignee.as_deref() != Some(target_id.as_str()) {
                    let target = self
                        .auth
                        .resolve_by_name(&target_id)?
                        .ok_or_else(|| anyhow!("unknown account '{name}'"))?;
                    let target_binding = self
                        .groups
                        .named_role(&self.project_of(channel), &target.nick)?;
                    if !target_binding.is_some_and(|other| {
                        !other.is_lead
                            && other.group.definition.name == binding.group.definition.name
                    }) {
                        return Err(anyhow!(
                            "@{} is not a named worker in group '{}'",
                            target.nick,
                            binding.group.definition.name
                        ));
                    }
                }
            }
        }
        if target_id != actor.id && !is_lead && !is_manager {
            return Err(anyhow!(
                "only a parent task's assignee or a leader or coordinator of {channel} can assign work to someone else"
            ));
        }
        Ok(target_id)
    }

    /// Reviewers and assignees both name existing project members.
    fn resolve_task_account(&self, channel: &str, name: &str) -> Result<String> {
        let project = self.project_of(channel);
        let channel = project.as_str();
        self.require_project(channel)?;
        let target = self
            .auth
            .resolve_by_name(name.trim().trim_start_matches('@'))?
            .ok_or_else(|| anyhow!("unknown account '{name}'"))?;
        if self.projects.role_of(channel, &target.id)?.is_none() {
            return Err(anyhow!(
                "@{} is not a member of {channel}; add them with POST /v1/projects/members first",
                target.nick
            ));
        }
        Ok(target.id)
    }

    /// A task's `repo` must name a git repo attached to the task's project channel.
    fn resolve_project_repo(&self, channel: &str, raw_repo: &str) -> Result<String> {
        let project = self.project_of(channel);
        let channel = project.as_str();
        self.require_project(channel)?;
        let repos = self.gits.list()?;
        let project_repos: Vec<&GitRepo> = repos.iter().filter(|r| r.channel == channel).collect();
        if let Some(matched) = project_repos
            .iter()
            .find(|r| r.name.eq_ignore_ascii_case(raw_repo))
        {
            return Ok(matched.name.clone());
        }
        if let Some(other) = repos.iter().find(|r| r.name.eq_ignore_ascii_case(raw_repo)) {
            if project_repos.is_empty() {
                return Err(anyhow!(
                    "repo '{raw_repo}' is registered for {}, not project {channel}; no git repos are attached to {channel}",
                    other.channel
                ));
            }
            let allowed = project_repos
                .iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(anyhow!(
                "repo '{raw_repo}' is registered for {}, not project {channel} (registered for {channel}: {allowed})",
                other.channel
            ));
        }
        if project_repos.is_empty() {
            return Err(anyhow!(
                "repo '{raw_repo}' is not registered for project {channel}; no git repos are attached to {channel} (register one with POST /v1/gits)"
            ));
        }
        let allowed = project_repos
            .iter()
            .map(|r| r.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        Err(anyhow!(
            "repo '{raw_repo}' is not registered for project {channel} (registered for {channel}: {allowed})"
        ))
    }

    pub fn create_task_full_api(&self, actor: &Principal, mut new: NewTask) -> Result<Task> {
        let title = new.title.trim().to_string();
        if title.is_empty() {
            return Err(anyhow!("task title is required"));
        }
        // A task named for a sprint room belongs to that room's project.
        // Otherwise it goes into the project's current sprint, if it has one.
        let requested = normalize_channel(&new.channel);
        let channel = self.project_of(&requested);
        new.sprint = if requested != channel {
            Some(requested.clone())
        } else {
            self.rooms.current(&channel)?.map(|room| room.channel)
        };
        if channel.eq_ignore_ascii_case(CHAT_ONLY_CHANNEL) {
            return Err(anyhow!(
                "{CHAT_ONLY_CHANNEL} is chat only; create the task in a project channel"
            ));
        }
        new.repo = match new
            .repo
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            Some(name) => Some(self.resolve_project_repo(&channel, name)?),
            None => None,
        };
        new.parent_task_id = new
            .parent_task_id
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty());
        let parent = new
            .parent_task_id
            .as_deref()
            .map(|id| self.get_task_api(id))
            .transpose()?;
        if let Some(parent) = &parent {
            if parent.channel != channel {
                return Err(anyhow!("parent task must belong to the same project"));
            }
            if TERMINAL_STATUSES.contains(&parent.status.as_str()) {
                return Err(anyhow!("cannot add subtasks to a {} parent", parent.status));
            }
            if new.repo.is_none() {
                new.repo = parent.repo.clone();
            }
            if requested == channel {
                new.sprint = parent.sprint.clone();
            }
        }
        let assignee_id = match new
            .assignee
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            Some(name) => Some(self.resolve_assignee(actor, &channel, name, parent.as_ref())?),
            None => None,
        };
        let reviewer = match new
            .reviewer
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
        {
            Some(name) => {
                let is_lead = parent
                    .as_ref()
                    .is_some_and(|task| task.assignee.as_deref() == Some(actor.id.as_str()));
                if !is_lead && !self.is_project_manager(actor, &channel)? {
                    return Err(anyhow!("only the parent task's assignee or a project manager can choose a reviewer"));
                }
                Some(self.resolve_task_account(&channel, name)?)
            }
            None => parent
                .as_ref()
                .and_then(|task| task.assignee.clone())
                .or(self.projects.get(&channel)?.and_then(|project| project.pm)),
        };
        new.channel = channel.clone();
        new.title = title;
        new.assignee = assignee_id.clone();
        new.reviewer = reviewer;
        new.created_by = Some(actor.id.clone());

        self.join_channels(actor, std::slice::from_ref(&channel))?;
        let task = self.tasks.create_full(new)?;
        let text = match &assignee_id {
            Some(assignee) => format!(
                "TASK {} created by @{} and assigned to @{}: {}",
                task.id, actor.nick, assignee, task.title
            ),
            None => format!(
                "TASK {} created by @{}: {}",
                task.id, actor.nick, task.title
            ),
        };
        self.broadcast_task_event(&task, "create", "success", &actor.nick, text)?;
        Ok(task)
    }

    pub fn get_task_api(&self, task_id: &str) -> Result<Task> {
        self.tasks
            .get(task_id.trim())?
            .ok_or_else(|| anyhow!("task '{}' not found", task_id.trim()))
    }

    /// Attach a test log or other evidence file to a task. The assignee, the
    /// reviewer, any member of the task's project and the owner may attach.
    pub fn attach_task_file_api(
        &self,
        actor: &Principal,
        task_id: &str,
        filename: &str,
        content_type: Option<&str>,
        kind: Option<&str>,
        note: Option<&str>,
        data: &[u8],
    ) -> Result<TaskAttachment> {
        let task = self.get_task_api(task_id)?;
        let project = self.project_of(&task.channel);
        let allowed = Self::is_owner(actor)
            || task.assignee.as_deref() == Some(actor.id.as_str())
            || task.reviewer.as_deref() == Some(actor.id.as_str())
            || self.projects.role_of(&project, &actor.id)?.is_some();
        if !allowed {
            return Err(anyhow!(
                "only members of {project}, the task's assignee or reviewer, or the owner can attach files to {}",
                task.id
            ));
        }
        self.attachments.add(NewAttachment {
            task_id: &task.id,
            filename,
            content_type,
            kind,
            note,
            uploaded_by: &actor.id,
            data,
        })
    }

    /// Assign, reassign or (with `None`) return a task to the pool.
    pub fn assign_task_api(
        &self,
        actor: &Principal,
        task_id: &str,
        assignee: Option<&str>,
    ) -> Result<Task> {
        let _guard = self.task_gate.lock().expect("task mutex poisoned");
        let current = self.get_task_api(task_id)?;
        let parent = current
            .parent_task_id
            .as_deref()
            .map(|id| self.get_task_api(id))
            .transpose()?;
        let assignee_id = match assignee.map(str::trim).filter(|name| !name.is_empty()) {
            Some(name) => {
                Some(self.resolve_assignee(actor, &current.channel, name, parent.as_ref())?)
            }
            None => {
                let is_owner = current.assignee.as_deref() == Some(actor.id.as_str());
                let is_lead = parent
                    .as_ref()
                    .is_some_and(|task| task.assignee.as_deref() == Some(actor.id.as_str()));
                if !is_owner && !is_lead && !self.is_project_manager(actor, &current.channel)? {
                    return Err(anyhow!(
                        "only the assignee or a project leader/coordinator can unassign a task"
                    ));
                }
                None
            }
        };
        let task = self
            .tasks
            .assign(&current.id, assignee_id.as_deref(), &actor.id)?;
        let text = match &task.assignee {
            Some(assignee) => format!(
                "TASK {} assigned to @{} by @{}: {}",
                task.id, assignee, actor.nick, task.title
            ),
            None => format!(
                "TASK {} returned to the pool by @{}: {}",
                task.id, actor.nick, task.title
            ),
        };
        self.broadcast_task_event(&task, "assign", "success", &actor.nick, text)?;
        Ok(task)
    }

    /// Move a task through its review workflow.
    ///
    /// The assignee starts work and asks for review; a project leader or
    /// coordinator oversees reviews. A named reviewer can decide their assigned
    /// reviews, but cannot approve their own work or cancel project tasks.
    #[allow(clippy::too_many_arguments)]
    #[cfg(test)]
    pub fn set_task_status_api(
        &self,
        actor: &Principal,
        task_id: &str,
        raw_to: &str,
        note: Option<&str>,
        branch: Option<&str>,
        report_body: Option<&str>,
        report_id: Option<&str>,
    ) -> Result<Task> {
        self.set_task_status_revision_api(
            actor,
            task_id,
            raw_to,
            note,
            branch,
            report_body,
            report_id,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn set_task_status_revision_api(
        &self,
        actor: &Principal,
        task_id: &str,
        raw_to: &str,
        note: Option<&str>,
        branch: Option<&str>,
        report_body: Option<&str>,
        report_id: Option<&str>,
        commit: Option<&str>,
    ) -> Result<Task> {
        let _guard = self.task_gate.lock().expect("task mutex poisoned");
        let current = self.get_task_api(task_id)?;
        let to = raw_to.trim().to_ascii_lowercase().replace('-', "_");
        let from = current.status.clone();
        let is_assignee = current.assignee.as_deref() == Some(actor.id.as_str());
        let is_manager = self.is_project_manager(actor, &current.channel)?;
        let is_reviewer = !is_assignee
            && current.reviewer.as_deref() == Some(actor.id.as_str())
            && self
                .projects
                .role_of(&current.channel, &actor.id)?
                .is_some();

        let allowed = match (from.as_str(), to.as_str()) {
            ("assigned" | "changes_requested" | "testing", "in_progress") => is_assignee,
            ("assigned" | "in_progress" | "changes_requested", "testing") => is_assignee,
            ("testing", "review") => is_assignee,
            ("review" | "super_review", "in_progress") => is_assignee,
            ("review" | "super_review", "changes_requested") => is_manager || is_reviewer,
            ("review", "merged") => is_manager || (is_reviewer && current.review_round.is_none()),
            ("super_review", "merged") => is_manager,
            ("review", "super_review") => is_manager && current.review_round.is_some(),
            (current_status, "cancelled") if !TERMINAL_STATUSES.contains(&current_status) => {
                is_manager
            }
            _ => false,
        };
        if !allowed {
            return Err(anyhow!(
                "cannot move task from {from} to {to}: the assignee moves work to in_progress, then testing, then review; the named reviewer or a project leader or coordinator moves review to merged or changes_requested"
            ));
        }

        let mut evidence: Vec<String> = note
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(str::to_string)
            .into_iter()
            .collect();
        let branch = branch.map(str::trim).filter(|b| !b.is_empty());
        if matches!(from.as_str(), "review" | "super_review")
            && current.review_round.is_some()
            && (branch.is_some_and(|b| Some(b) != current.branch.as_deref()) || commit.is_some())
        {
            return Err(anyhow!("submitted branch and commit are immutable; return to in_progress, test and resubmit"));
        }
        let mut report_commit = None;
        // Whoever reviews reads the evidence instead of re-running the tests.
        if from == "testing" && to == "review" {
            if let Some(body) = report_body.map(str::trim).filter(|b| !b.is_empty()) {
                evidence.push(body.to_string());
            }
            if let Some(id) = report_id.map(str::trim).filter(|id| !id.is_empty()) {
                let report = self
                    .reports
                    .get(id)?
                    .ok_or_else(|| anyhow!("report '{id}' not found"))?;
                if report.author_id != actor.id
                    || report.task_id.as_deref() != Some(current.id.as_str())
                    || self.project_of(&report.channel) != current.channel
                    || report.body.trim().is_empty()
                {
                    return Err(anyhow!("test report must have a nonblank body, be authored by the assignee and belong to this task and project"));
                }
                evidence.push(format!("Test report {}:\n{}", report.id, report.body));
                report_commit = report.git_commit;
            }
            if evidence.is_empty() {
                return Err(anyhow!(
                    "a test report is required to hand work in: pass what you ran and the result as note, report_body or a task-linked report_id"
                ));
            }
        }
        let mut evidence = evidence.join("\n\n");
        let round = if from == "testing" && to == "review" {
            self.new_review_round(
                &current,
                branch.or(current.branch.as_deref()),
                commit.or(report_commit.as_deref()),
                &evidence,
            )?
        } else {
            None
        };
        if round.is_some()
            && commit
                .zip(report_commit.as_deref())
                .is_some_and(|(submitted, reported)| {
                    !submitted.trim().eq_ignore_ascii_case(reported.trim())
                })
        {
            return Err(anyhow!(
                "submitted commit differs from the linked test report commit"
            ));
        }
        if let Some(round) = current
            .review_round
            .as_ref()
            .filter(|_| is_manager && matches!(to.as_str(), "merged" | "super_review"))
        {
            // The approved PM/owner override is explicit in the audit history.
            let eligible = self.eligible_review_peers(&current, round)?;
            if !round.consensus(&eligible) {
                if evidence.is_empty() {
                    return Err(anyhow!(
                        "a note explaining the PM/owner consensus bypass is required"
                    ));
                }
                evidence = format!("PM/owner consensus bypass: {evidence}");
            }
        }
        let note = (!evidence.is_empty()).then_some(evidence.as_str());
        let task = self.tasks.set_status_with_round(
            &current.id,
            &from,
            &to,
            &actor.id,
            note,
            branch,
            round.as_ref(),
        )?;

        let mut text = format!(
            "TASK {} {} -> {} by @{}: {}",
            task.id, from, to, actor.nick, task.title
        );
        if let Some(note) = note {
            text.push_str(&format!(" - {note}"));
        }
        self.broadcast_task_event(&task, "status", "success", &actor.nick, text)?;
        Ok(task)
    }

    fn new_review_round(
        &self,
        task: &Task,
        branch: Option<&str>,
        commit: Option<&str>,
        evidence: &str,
    ) -> Result<Option<ReviewRound>> {
        let assignee = self
            .auth
            .resolve_by_name(task.assignee.as_deref().unwrap_or_default())?
            .ok_or_else(|| anyhow!("task assignee not found"))?;
        let mut selected = None;
        for group in self.groups.list()? {
            if group.definition.review_policy.is_none() || !group.projects.contains(&task.channel) {
                continue;
            }
            let named = std::iter::once(&group.definition.lead)
                .chain(&group.definition.workers)
                .any(|member| {
                    member.nick.as_deref().is_some_and(|nick| {
                        nick.trim_start_matches('@')
                            .eq_ignore_ascii_case(&assignee.nick)
                    })
                });
            if named && selected.replace(group).is_some() {
                return Err(anyhow!(
                    "ambiguous group consensus policy for the task assignee"
                ));
            }
        }
        let Some(group) = selected else {
            return Ok(None);
        };
        let policy = group.definition.review_policy.as_ref().unwrap();
        let mut peers = Vec::new();
        for nick in &policy.peer_reviewers {
            let id = self.resolve_task_account(&task.channel, nick)?;
            if id != assignee.id && !peers.contains(&id) {
                peers.push(id);
            }
        }
        let quorum = policy.quorum.map(|q| q as usize).unwrap_or(peers.len());
        if quorum == 0 || quorum > peers.len() {
            return Err(anyhow!("review quorum cannot be met after excluding the assignee; configure enough named peers"));
        }
        let commit = commit.unwrap_or_default().trim();
        if !matches!(commit.len(), 40 | 64) || !commit.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(anyhow!("group peer review requires a full commit hash via commit or the linked test report"));
        }
        let branch = branch
            .map(str::trim)
            .filter(|b| !b.is_empty())
            .ok_or_else(|| anyhow!("group peer review requires a submitted branch"))?;
        Ok(Some(ReviewRound {
            revision: format!("review_{}", uuid::Uuid::new_v4().simple()),
            group: group.definition.name,
            assignee: assignee.id,
            commit: commit.to_ascii_lowercase(),
            branch: branch.to_string(),
            evidence: evidence.to_string(),
            peer_reviewers: peers,
            quorum,
            votes: Vec::new(),
        }))
    }

    fn eligible_review_peers(&self, task: &Task, round: &ReviewRound) -> Result<Vec<String>> {
        let Some(group) = self.groups.get(&round.group)? else {
            return Ok(Vec::new());
        };
        if !group.projects.contains(&task.channel) {
            return Ok(Vec::new());
        }
        let mut eligible = Vec::new();
        for id in &round.peer_reviewers {
            if task.assignee.as_ref() == Some(id)
                || self.projects.role_of(&task.channel, id)?.is_none()
            {
                continue;
            }
            let Some(peer) = self.auth.resolve_by_name(id)? else {
                continue;
            };
            if std::iter::once(&group.definition.lead)
                .chain(&group.definition.workers)
                .any(|member| {
                    member.nick.as_deref().is_some_and(|nick| {
                        nick.trim_start_matches('@')
                            .eq_ignore_ascii_case(&peer.nick)
                    })
                })
            {
                eligible.push(id.clone());
            }
        }
        Ok(eligible)
    }

    pub fn peer_review_task_api(
        &self,
        actor: &Principal,
        task_id: &str,
        revision: &str,
        decision: &str,
        note: &str,
    ) -> Result<Task> {
        let _guard = self.task_gate.lock().expect("task mutex poisoned");
        let current = self.get_task_api(task_id)?;
        let round = current
            .review_round
            .as_ref()
            .ok_or_else(|| anyhow!("task has no peer review round"))?;
        let eligible = self.eligible_review_peers(&current, round)?;
        let task =
            self.tasks
                .peer_vote(&current.id, revision, &actor.id, decision, note, &eligible)?;
        self.broadcast_task_event(
            &task,
            "peer_review",
            "success",
            &actor.nick,
            format!(
                "TASK {} peer {} by @{} for {}: {} - {}",
                task.id, decision, actor.nick, revision, task.status, note
            ),
        )?;
        Ok(task)
    }

    /// Everything an account has to do, grouped by project, with the repo
    /// each task belongs to. This is the one call an agent makes on wake-up.
    pub fn inbox_api(&self, actor: &Principal) -> Result<serde_json::Value> {
        let mine = self.tasks.inbox(&actor.id)?;
        let roles: HashMap<String, String> =
            self.projects.projects_of(&actor.id)?.into_iter().collect();
        let repos = self.gits.list()?;

        let mut channels: std::collections::BTreeSet<String> =
            mine.iter().map(|t| t.channel.clone()).collect();
        channels.extend(roles.keys().cloned());

        let mut projects = Vec::new();
        let mut review_count = 0usize;
        let mut question_count = 0usize;
        for channel in channels {
            let my_role = roles.get(&channel).cloned();
            let tasks: Vec<&Task> = mine.iter().filter(|t| t.channel == channel).collect();
            let is_manager = my_role
                .as_deref()
                .is_some_and(|role| MANAGER_ROLES.contains(&role))
                || Self::is_owner(actor);
            let mut to_review = Vec::new();
            for task in self.tasks.review_queue(&channel)? {
                let routed = if let Some(round) = &task.review_round {
                    is_manager
                        || (self
                            .eligible_review_peers(&task, round)?
                            .contains(&actor.id)
                            && !round
                                .votes
                                .iter()
                                .any(|v| v.reviewer == actor.id && v.decision == "approve"))
                } else {
                    is_manager
                        || (my_role.is_some()
                            && task.reviewer.as_deref() == Some(actor.id.as_str())
                            && task.assignee.as_deref() != Some(actor.id.as_str()))
                };
                if routed {
                    to_review.push(task);
                }
            }
            // Questions from blocked workers that this account should answer.
            let mut questions = Vec::new();
            for question in self.tasks.questions(Some(&channel), "open")? {
                if is_manager
                    || self.is_task_reviewer(actor, &self.get_task_api(&question.task_id)?)?
                {
                    questions.push(question);
                }
            }
            if is_manager {
                questions.extend(self.tasks.questions(Some(&channel), "escalated_pm")?);
            }
            if tasks.is_empty() && to_review.is_empty() && questions.is_empty() {
                continue;
            }
            review_count += to_review.len();
            question_count += questions.len();
            let channel_repos: Vec<&GitRepo> =
                repos.iter().filter(|r| r.channel == channel).collect();
            let project = self.projects.get(&channel)?;
            projects.push(serde_json::json!({
                "channel": channel,
                "project": project.as_ref().map(|p| p.name.clone()),
                "pm": project.and_then(|p| p.pm),
                "my_role": my_role,
                "repos": channel_repos,
                "tasks": tasks,
                "to_review": to_review,
                "questions": questions,
            }));
        }

        // What only the owner can decide, across every project.
        let needs_you = if Self::is_owner(actor) {
            self.tasks.questions(None, "escalated")?
        } else {
            Vec::new()
        };

        Ok(serde_json::json!({
            "account": actor.id,
            "task_count": mine.len(),
            "review_count": review_count,
            "question_count": question_count,
            "needs_you": needs_you,
            "projects": projects,
        }))
    }

    pub fn claim_task_api(&self, actor: &Principal, task_id: &str) -> Result<Task> {
        let task = self.tasks.claim(task_id.trim(), &actor.id)?;
        self.broadcast_task_event(
            &task,
            "claim",
            "success",
            &actor.nick,
            format!(
                "TASK {} claimed by @{}: {}",
                task.id, actor.nick, task.title
            ),
        )?;
        Ok(task)
    }

    pub fn done_task_api(&self, actor: &Principal, task_id: &str) -> Result<Task> {
        let task = self.tasks.done(task_id.trim(), &actor.id)?;
        self.broadcast_task_event(
            &task,
            "done",
            "success",
            &actor.nick,
            format!(
                "TASK {} completed by @{}: {}",
                task.id, actor.nick, task.title
            ),
        )?;
        Ok(task)
    }

    pub fn release_task_api(&self, actor: &Principal, task_id: &str) -> Result<Task> {
        let task = self.tasks.release(task_id.trim(), &actor.id)?;
        self.broadcast_task_event(
            &task,
            "release",
            "success",
            &actor.nick,
            format!(
                "TASK {} released by @{}: {}",
                task.id, actor.nick, task.title
            ),
        )?;
        Ok(task)
    }

    pub fn set_role_api(
        &self,
        actor: &Principal,
        target_name: &str,
        role: &str,
        channel: Option<&str>,
    ) -> Result<Principal> {
        let updated = self.auth.set_role(target_name, role)?;
        let announce_channel = channel
            .map(normalize_channel)
            .or_else(|| {
                self.memberships(&actor.id)
                    .ok()
                    .and_then(|m| m.into_iter().next().map(|x| x.channel))
            })
            .unwrap_or_else(|| "#general".to_string());
        self.join_channels(actor, std::slice::from_ref(&announce_channel))?;

        let notice_body = format!(
            "ROLE @{} set to '{}' by @{}",
            updated.nick, updated.role, actor.nick
        );
        let message = self
            .history
            .append(&announce_channel, &actor.id, &notice_body)?;
        let _ = self.mark_seen(&actor.id, &announce_channel, message.seq);
        let tag_str = format_message_tags(&[
            ("seq", message.seq.to_string()),
            ("msg-id", message.msg_id.clone()),
            ("time", message.created_at.to_string()),
            ("role-target", updated.nick.clone()),
            ("role-value", updated.role.clone()),
        ]);
        self.broadcast_message(
            &announce_channel,
            format!("{tag_str} :{SERVER_NAME} NOTICE {announce_channel} :{notice_body}"),
            message.seq,
        )?;
        self.logger.log_security(
            Severity::Info,
            "role.changed",
            "success",
            Some(&actor.id),
            Some(&updated.id),
            Some(&announce_channel),
            notice_body.clone(),
        );
        self.emit_webhook(WebhookEvent {
            event: "role".to_string(),
            channel: announce_channel,
            seq: Some(message.seq),
            msg_id: Some(message.msg_id),
            from: actor.id.clone(),
            to: Some(updated.id.clone()),
            message: notice_body,
            created_at: message.created_at,
            report: None,
            task: None,
            git: None,
            lock: serde_json::to_value(&updated).ok(),
        });
        Ok(updated)
    }

    pub fn acquire_lock_api(
        &self,
        actor: &Principal,
        resource: &str,
        raw_channel: &str,
        note: Option<&str>,
        ttl_seconds: Option<i64>,
    ) -> Result<ResourceLock> {
        let channel = normalize_channel(raw_channel);
        let lock = self
            .locks
            .acquire(resource, &actor.id, &channel, note, ttl_seconds)?;
        self.join_channels(actor, std::slice::from_ref(&channel))?;

        let note_suffix = lock
            .note
            .as_deref()
            .map(|n| format!(" ({n})"))
            .unwrap_or_default();
        let notice_body = format!(
            "LOCK {} acquired by @{} until {}{}",
            lock.resource, lock.owner_nick, lock.expires_at, note_suffix
        );
        let message = self.history.append(&channel, &actor.id, &notice_body)?;
        let _ = self.mark_seen(&actor.id, &channel, message.seq);
        let tag_str = format_message_tags(&[
            ("seq", message.seq.to_string()),
            ("msg-id", message.msg_id.clone()),
            ("time", message.created_at.to_string()),
            ("lock-resource", lock.resource.clone()),
            ("lock-action", "acquire".to_string()),
            ("lock-owner", lock.owner_nick.clone()),
        ]);
        self.broadcast_message(
            &channel,
            format!("{tag_str} :{SERVER_NAME} NOTICE {channel} :{notice_body}"),
            message.seq,
        )?;
        self.emit_webhook(WebhookEvent {
            event: "lock".to_string(),
            channel,
            seq: Some(message.seq),
            msg_id: Some(message.msg_id),
            from: actor.id.clone(),
            to: None,
            message: notice_body,
            created_at: message.created_at,
            report: None,
            task: None,
            git: None,
            lock: serde_json::to_value(&lock).ok(),
        });
        Ok(lock)
    }

    pub fn release_lock_api(
        &self,
        actor: &Principal,
        resource: &str,
        force: bool,
    ) -> Result<Option<ResourceLock>> {
        let released = self.locks.release(resource, &actor.id, force)?;
        let Some(lock) = released else {
            return Ok(None);
        };
        let channel = normalize_channel(&lock.channel);
        let notice_body = format!(
            "UNLOCK {} released by @{} (was held by @{})",
            lock.resource, actor.nick, lock.owner_nick
        );
        let message = self.history.append(&channel, &actor.id, &notice_body)?;
        let _ = self.mark_seen(&actor.id, &channel, message.seq);
        let tag_str = format_message_tags(&[
            ("seq", message.seq.to_string()),
            ("msg-id", message.msg_id.clone()),
            ("time", message.created_at.to_string()),
            ("lock-resource", lock.resource.clone()),
            ("lock-action", "release".to_string()),
            ("lock-owner", actor.nick.clone()),
        ]);
        self.broadcast_message(
            &channel,
            format!("{tag_str} :{SERVER_NAME} NOTICE {channel} :{notice_body}"),
            message.seq,
        )?;
        self.emit_webhook(WebhookEvent {
            event: "lock".to_string(),
            channel,
            seq: Some(message.seq),
            msg_id: Some(message.msg_id),
            from: actor.id.clone(),
            to: None,
            message: notice_body,
            created_at: message.created_at,
            report: None,
            task: None,
            git: None,
            lock: serde_json::to_value(&lock).ok(),
        });
        Ok(Some(lock))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn post_git_event(
        &self,
        actor_id: &str,
        actor_nick: &str,
        raw_repo_name: &str,
        explicit_channel: Option<&str>,
        event_kind: &str,
        branch: Option<&str>,
        commit: Option<&str>,
        title: Option<&str>,
        url: Option<&str>,
        summary: &str,
    ) -> Result<(StoredMessage, String)> {
        let clean_repo = sanitize_repo_name(raw_repo_name)
            .unwrap_or_else(|_| raw_repo_name.trim().to_ascii_lowercase());
        let registered = self.gits.get_with_secret(&clean_repo)?;
        let channel = explicit_channel
            .map(normalize_channel)
            .or_else(|| registered.as_ref().map(|(r, _)| r.channel.clone()))
            .unwrap_or_else(|| format!("#{clean_repo}"));

        if registered.is_some() {
            let _ = self.gits.record_event(&clean_repo, branch, commit);
        }

        let branch_part = branch
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|b| format!(" [{b}]"))
            .unwrap_or_default();
        let notice_body =
            format!("[git:{clean_repo}:{event_kind}]{branch_part} by @{actor_nick}: {summary}");
        let message = self.history.append(&channel, actor_id, &notice_body)?;
        let _ = self.mark_seen(actor_id, &channel, message.seq);

        let mut tags = vec![
            ("seq", message.seq.to_string()),
            ("msg-id", message.msg_id.clone()),
            ("time", message.created_at.to_string()),
            ("git-repo", clean_repo.clone()),
            ("git-event", event_kind.to_string()),
        ];
        if let Some(b) = branch.filter(|s| !s.trim().is_empty()) {
            tags.push(("git-branch", b.trim().to_string()));
        }
        if let Some(c) = commit.filter(|s| !s.trim().is_empty()) {
            tags.push(("git-commit", c.trim().to_string()));
        }
        let tag_str = format_message_tags(&tags);
        self.broadcast_message(
            &channel,
            format!("{tag_str} :{SERVER_NAME} NOTICE {channel} :{notice_body}"),
            message.seq,
        )?;

        let git_meta = serde_json::json!({
            "repo": clean_repo,
            "event": event_kind,
            "branch": branch,
            "commit": commit,
            "title": title,
            "url": url,
            "actor": actor_nick,
            "summary": summary,
        });
        self.emit_webhook(WebhookEvent {
            event: "git".to_string(),
            channel: channel.clone(),
            seq: Some(message.seq),
            msg_id: Some(message.msg_id.clone()),
            from: actor_id.to_string(),
            to: None,
            message: notice_body,
            created_at: message.created_at,
            report: None,
            task: None,
            git: Some(git_meta),
            lock: None,
        });

        Ok((message, channel))
    }

    pub fn directory_snapshot(&self) -> Result<DirectorySnapshot> {
        let online = self.online_principal_ids();
        let configured_webhooks = self.webhooks.configured_principals()?;
        let agents = self
            .auth
            .list_principals()?
            .into_iter()
            .map(|p| AgentDirectoryEntry {
                online: online.contains(&p.id),
                webhook_configured: configured_webhooks.contains(&p.id),
                id: p.id,
                nick: p.nick,
                team: p.team,
                role: p.role,
            })
            .collect();

        let channels = self
            .history
            .recent_channels()?
            .into_iter()
            .map(
                |(channel, message_count, last_activity_at)| ChannelSummary {
                    channel,
                    message_count,
                    last_activity_at,
                },
            )
            .collect();

        let git_repos = self.gits.list()?;
        let locks = self.locks.list(None)?;

        Ok(DirectorySnapshot {
            agents,
            channels,
            git_repos,
            locks,
        })
    }

    fn deliver_channel_message(
        &self,
        sender: &Principal,
        channel: &str,
        body: &str,
        exclude_session_id: Option<&str>,
    ) -> Result<StoredMessage> {
        let message = self.history.append(channel, &sender.id, body)?;
        // Always advance the sender's read cursor so suppressed self-echoes aren't replayed later.
        let _ = self.mark_seen(&sender.id, channel, message.seq);
        let tags = metadata_tags(message.seq, &message.msg_id, message.created_at, false);
        self.broadcast_message_excluding(
            channel,
            format!("{tags} :{} PRIVMSG {} :{}", sender.nick, channel, body),
            message.seq,
            exclude_session_id,
        )?;
        self.emit_webhook(WebhookEvent {
            event: "message".to_string(),
            channel: channel.to_string(),
            seq: Some(message.seq),
            msg_id: Some(message.msg_id.clone()),
            from: sender.id.clone(),
            to: None,
            message: body.to_string(),
            created_at: message.created_at,
            report: None,
            task: None,
            git: None,
            lock: None,
        });
        Ok(message)
    }

    fn deliver_direct_message(
        &self,
        sender: &Principal,
        peer: &Principal,
        body: &str,
    ) -> Result<StoredMessage> {
        let dm_channel = canonical_dm_channel(&sender.id, &peer.id);
        let message = self.history.append(&dm_channel, &sender.id, body)?;
        let tags = metadata_tags(message.seq, &message.msg_id, message.created_at, false);
        let peer_client = {
            let inner = self.inner.lock().expect("server mutex poisoned");
            inner.clients.get(&peer.id).cloned()
        };
        if let Some(client) = peer_client {
            if client
                .tx
                .send(format!(
                    "{tags} :{} PRIVMSG {} :{}",
                    sender.nick, peer.nick, body
                ))
                .is_ok()
            {
                let _ = self.mark_seen(&peer.id, &dm_channel, message.seq);
            }
        }
        self.emit_webhook(WebhookEvent {
            event: "dm".to_string(),
            channel: dm_channel,
            seq: Some(message.seq),
            msg_id: Some(message.msg_id.clone()),
            from: sender.id.clone(),
            to: Some(peer.id.clone()),
            message: body.to_string(),
            created_at: message.created_at,
            report: None,
            task: None,
            git: None,
            lock: None,
        });
        Ok(message)
    }

    fn emit_webhook(&self, event: WebhookEvent) {
        self.emit_webhook_to(event, None);
    }

    fn emit_webhook_to(&self, event: WebhookEvent, task_recipients: Option<&HashSet<String>>) {
        let online = self.online_principal_ids();
        let members = if event.channel.starts_with('#') || event.channel.starts_with('&') {
            self.channel_member_ids(&event.channel).unwrap_or_default()
        } else {
            Vec::new()
        };
        let role_mentions = self.role_mentions(&event.channel, &event.message);
        self.request_wakes(&event, &members, &role_mentions, task_recipients);
        if let Err(err) = self
            .webhooks
            .enqueue_for_event(&event, &online, &members, &role_mentions)
        {
            self.logger.log_error(
                "webhook.enqueue_error",
                format!("enqueue webhook error ({}): {err:#}", event.event),
            );
        }
    }

    /// Start the configured agent of every account this event addresses:
    /// a direct mention, a role it holds in the project, `@here` in a channel
    /// it belongs to, or a direct message.
    fn request_wakes(
        &self,
        event: &WebhookEvent,
        members: &[String],
        role_mentions: &HashSet<String>,
        task_recipients: Option<&HashSet<String>>,
    ) {
        let enabled = match self.wakes.enabled_principals() {
            Ok(list) if !list.is_empty() => list,
            _ => return,
        };
        let mentions = crate::webhooks::extract_mentions(&event.message);
        let everyone = crate::webhooks::GROUP_MENTIONS
            .iter()
            .any(|tag| mentions.contains(*tag));
        let sender = event.from.to_ascii_lowercase();
        let task_id = event
            .task
            .as_ref()
            .and_then(|task| task.get("id"))
            .and_then(|id| id.as_str())
            .map(str::to_owned);
        let task_repo = event
            .task
            .as_ref()
            .and_then(|task| task.get("repo"))
            .and_then(|repo| repo.as_str())
            .map(str::to_owned);

        for principal_id in enabled {
            let lowered = principal_id.to_ascii_lowercase();
            if lowered == sender {
                continue;
            }
            let is_dm = event.event == "dm"
                && event
                    .to
                    .as_deref()
                    .is_some_and(|to| to.eq_ignore_ascii_case(&principal_id));
            if event.event == "dm" && !is_dm {
                continue;
            }
            let addressed = if let Some(recipients) = task_recipients {
                recipients.contains(&lowered)
            } else {
                is_dm
                    || mentions.contains(&lowered)
                    || role_mentions.contains(&lowered)
                    || (everyone
                        && members
                            .iter()
                            .any(|m| m.eq_ignore_ascii_case(&principal_id)))
            };
            if !addressed {
                continue;
            }
            let summary: String = event
                .message
                .chars()
                .map(|c| if c.is_control() { ' ' } else { c })
                .take(300)
                .collect();
            let ctx = WakeContext {
                nick: self
                    .display_name(&principal_id)
                    .unwrap_or_else(|_| principal_id.clone()),
                is_pm: self.is_pm_of(&event.channel, &principal_id),
                principal_id,
                reason: format!(
                    "{} in {} from @{}: {}",
                    event.event, event.channel, event.from, summary
                ),
                channel: event.channel.clone(),
                task_id: task_id.clone(),
                repo_path: self.repo_path_for(&event.channel, task_repo.as_deref()),
                event: Some(event.event.clone()),
                triggered_by: Some(event.from.clone()),
            };
            if let Err(err) = self.wakes.request(ctx) {
                self.logger
                    .log_error("wake.request_error", format!("wake request error: {err:#}"));
            }
        }
    }

    /// The local folder of the task's repo, or of the project's first repo.
    fn repo_path_for(&self, channel: &str, repo_name: Option<&str>) -> Option<String> {
        let project = self.project_of(channel);
        let channel = project.as_str();
        let repos = self.gits.list().ok()?;
        repo_name
            .and_then(|name| {
                repos
                    .iter()
                    .find(|r| r.channel == channel && r.name.eq_ignore_ascii_case(name))
            })
            .or_else(|| repos.iter().find(|r| r.channel == channel))
            .map(|r| r.local_path.clone())
    }

    fn is_pm_of(&self, channel: &str, principal_id: &str) -> bool {
        let project = self.project_of(channel);
        let channel = project.as_str();
        self.projects
            .get(channel)
            .ok()
            .flatten()
            .and_then(|project| project.pm)
            .is_some_and(|pm| pm.eq_ignore_ascii_case(principal_id))
    }

    /// Owner-triggered launch, used to check an account's wake configuration
    /// or to hand it an instruction. `channel` launches it in that project.
    pub fn wake_account_api(
        &self,
        target_name: &str,
        reason: &str,
        channel: Option<&str>,
    ) -> Result<bool> {
        let target = self
            .auth
            .resolve_by_name(target_name.trim().trim_start_matches('@'))?
            .ok_or_else(|| anyhow!("unknown account '{target_name}'"))?;
        let channel = channel
            .map(normalize_channel)
            .unwrap_or_else(|| CHAT_ONLY_CHANNEL.to_string());
        self.wakes.request(WakeContext {
            is_pm: self.is_pm_of(&channel, &target.id),
            repo_path: self.repo_path_for(&channel, None),
            principal_id: target.id,
            nick: target.nick,
            reason: reason.to_string(),
            channel,
            task_id: None,
            event: Some("owner".to_string()),
            triggered_by: None,
        })
    }

    /// `@coordinator` or `@leader` in a project channel addresses whoever
    /// holds that role in that project.
    fn role_mentions(&self, channel: &str, message: &str) -> HashSet<String> {
        let project = self.project_of(channel);
        let channel = project.as_str();
        let mut out = HashSet::new();
        let words = crate::webhooks::extract_mentions(message);
        if words.is_empty() {
            return out;
        }
        if let Ok(members) = self.projects.members(channel) {
            for member in members {
                if words.contains(&member.role) {
                    out.insert(member.principal_id.to_ascii_lowercase());
                }
            }
        }
        out
    }

    fn online_principal_ids(&self) -> HashSet<String> {
        self.inner
            .lock()
            .expect("server mutex poisoned")
            .clients
            .keys()
            .cloned()
            .collect()
    }

    fn channel_member_ids(&self, channel: &str) -> Result<Vec<String>> {
        let db = self.db.read()?;
        let mut stmt =
            db.prepare("SELECT principal_id FROM channel_memberships WHERE channel = ?1")?;
        let rows = stmt
            .query_map(params![channel], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn channel_member_directory(&self, channel: &str) -> Result<Vec<(String, String, String)>> {
        let db = self.db.read()?;
        let mut stmt = db.prepare(
            "SELECT m.principal_id, COALESCE(p.nick, m.principal_id), COALESCE(p.role, 'member')
             FROM channel_memberships m
             LEFT JOIN principals p ON p.id = m.principal_id
             WHERE m.channel = ?1
             ORDER BY LOWER(COALESCE(p.nick, m.principal_id)) ASC",
        )?;
        let rows = stmt
            .query_map(params![channel], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn broadcast_raw(&self, channel: &str, line: String) -> Result<()> {
        let recipients = self.recipients(channel);
        for recipient in recipients {
            let _ = recipient.tx.send(line.clone());
        }
        Ok(())
    }

    fn broadcast_message(&self, channel: &str, line: String, seq: i64) -> Result<()> {
        self.broadcast_message_excluding(channel, line, seq, None)
    }

    fn broadcast_message_excluding(
        &self,
        channel: &str,
        line: String,
        seq: i64,
        exclude_session_id: Option<&str>,
    ) -> Result<()> {
        let recipients = self.recipients_with_ids(channel);
        for (principal_id, recipient) in recipients {
            if exclude_session_id == Some(recipient.session_id.as_str()) {
                continue;
            }
            if recipient.tx.send(line.clone()).is_ok() {
                self.mark_seen(&principal_id, channel, seq)?;
            }
        }
        Ok(())
    }

    fn recipients(&self, channel: &str) -> Vec<ClientHandle> {
        self.recipients_with_ids(channel)
            .into_iter()
            .map(|(_, client)| client)
            .collect()
    }

    fn recipients_with_ids(&self, channel: &str) -> Vec<(String, ClientHandle)> {
        let inner = self.inner.lock().expect("server mutex poisoned");
        let Some(members) = inner.channels.get(channel) else {
            return Vec::new();
        };

        members
            .iter()
            .filter_map(|principal_id| {
                inner
                    .clients
                    .get(principal_id)
                    .cloned()
                    .map(|client| (principal_id.clone(), client))
            })
            .collect()
    }

    fn disconnect(&self, session: &Session) -> Result<()> {
        let _guard = self.session_gate.lock().expect("session mutex poisoned");
        let Some(principal) = session.principal.as_ref() else {
            return Ok(());
        };

        let should_mark_disconnected = {
            let mut inner = self.inner.lock().expect("server mutex poisoned");
            let should_remove = inner
                .clients
                .get(&principal.id)
                .map(|client| client.tx.same_channel(&session.tx))
                // Reissue already removed the client; still record its exit.
                .unwrap_or(true);
            if should_remove {
                inner.clients.remove(&principal.id);
            }
            should_remove
        };

        if should_mark_disconnected {
            let db = self.db.write()?;
            db.execute(
                "UPDATE principals SET disconnected_at = ?1 WHERE id = ?2",
                params![unix_timestamp(), principal.id],
            )?;
        }

        Ok(())
    }

    fn ensure_registered(&self, session: &Session) -> Result<bool> {
        if !session.registered {
            session.send(format!(":{SERVER_NAME} 451 * :You have not registered"));
            return Ok(false);
        }

        let principal = session.principal()?;
        let is_active = self
            .inner
            .lock()
            .expect("server mutex poisoned")
            .clients
            .get(&principal.id)
            .is_some_and(|client| client.session_id == session.session_id);

        if !is_active {
            session.send(format!(
                ":{SERVER_NAME} ERROR :Session replaced by a newer connection"
            ));
            return Err(anyhow!("session replaced"));
        }

        if let Some(info) = session.token_info.as_ref() {
            let expired_or_revoked = match info.token_id.as_deref() {
                Some(token_id) => self.auth.is_token_active(token_id)?.is_none(),
                None => info.is_expired_at(unix_timestamp()),
            };
            if expired_or_revoked {
                self.logger.log_security(
                    Severity::Warning,
                    "auth.irc_expired",
                    "failure",
                    Some(&principal.id),
                    None,
                    Some("irc"),
                    format!("Active IRC session token expired for @{}", principal.nick),
                );
                session.send(format!(":{SERVER_NAME} ERROR :Bearer token has expired"));
                return Err(anyhow!("token expired"));
            }
        }

        Ok(true)
    }

    fn require_irc_scope(
        &self,
        session: &Session,
        required: Scope,
        command_name: &str,
    ) -> Result<bool> {
        if !self.ensure_registered(session)? {
            return Ok(false);
        }
        if session.has_scope(required) {
            return Ok(true);
        }
        let principal = session.principal()?;
        self.logger.log_security(
            Severity::Warning,
            "auth.scope_denied",
            "failure",
            Some(&principal.id),
            None,
            Some("irc"),
            format!(
                "IRC {command_name} denied for @{}: token lacks required scope '{}'",
                principal.nick,
                required.as_str()
            ),
        );
        session.send(format!(
            ":{SERVER_NAME} 481 {} :Permission denied - {command_name} requires token scope '{}'",
            principal.nick,
            required.as_str()
        ));
        Ok(false)
    }

    fn mark_connected_and_memberships(&self, principal_id: &str) -> Result<Vec<Membership>> {
        let now = unix_timestamp();
        let db = self.db.write()?;
        db.execute(
            "UPDATE principals SET connected_at = ?1, disconnected_at = NULL WHERE id = ?2",
            params![now, principal_id],
        )?;

        let mut statement = db.prepare(
            "SELECT channel, last_seen_seq FROM channel_memberships WHERE principal_id = ?1 ORDER BY channel",
        )?;
        let memberships = statement
            .query_map(params![principal_id], |row| {
                Ok(Membership {
                    channel: row.get(0)?,
                    last_seen_seq: row.get(1)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(memberships)
    }

    fn memberships(&self, principal_id: &str) -> Result<Vec<Membership>> {
        let db = self.db.read()?;
        let mut statement = db.prepare(
            "SELECT channel, last_seen_seq FROM channel_memberships WHERE principal_id = ?1 ORDER BY channel",
        )?;
        let memberships = statement
            .query_map(params![principal_id], |row| {
                Ok(Membership {
                    channel: row.get(0)?,
                    last_seen_seq: row.get(1)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(memberships)
    }

    fn add_membership(&self, principal_id: &str, channel: &str) -> Result<()> {
        let db = self.db.write()?;
        let current_channel_seq: i64 = db.query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM messages WHERE channel = ?1",
            params![channel],
            |row| row.get(0),
        )?;
        db.execute(
            "INSERT OR IGNORE INTO channel_memberships (principal_id, channel, joined_at, last_seen_seq)
             VALUES (?1, ?2, ?3, ?4)",
            params![principal_id, channel, unix_timestamp(), current_channel_seq],
        )?;
        Ok(())
    }

    fn remove_membership(&self, principal_id: &str, channel: &str) -> Result<()> {
        let db = self.db.write()?;
        db.execute(
            "DELETE FROM channel_memberships WHERE principal_id = ?1 AND channel = ?2",
            params![principal_id, channel],
        )?;
        Ok(())
    }

    fn is_member(&self, principal_id: &str, channel: &str) -> Result<bool> {
        let db = self.db.read()?;
        let count: i64 = db.query_row(
            "SELECT COUNT(*) FROM channel_memberships WHERE principal_id = ?1 AND channel = ?2",
            params![principal_id, channel],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    fn display_name(&self, principal_id: &str) -> Result<String> {
        if principal_id == SERVER_NAME {
            return Ok(SERVER_NAME.to_string());
        }

        let db = self.db.read()?;
        let nick = db.query_row(
            "SELECT nick FROM principals WHERE id = ?1",
            params![principal_id],
            |row| row.get(0),
        );

        Ok(nick.unwrap_or_else(|_| principal_id.to_string()))
    }

    fn mark_seen(&self, principal_id: &str, channel: &str, seq: i64) -> Result<()> {
        let db = self.db.write()?;
        db.execute(
            "UPDATE principals
             SET last_seen_seq = MAX(last_seen_seq, ?1)
             WHERE id = ?2",
            params![seq, principal_id],
        )?;
        db.execute(
            "UPDATE channel_memberships
             SET last_seen_seq = MAX(last_seen_seq, ?1)
             WHERE principal_id = ?2 AND channel = ?3",
            params![seq, principal_id, channel],
        )?;
        Ok(())
    }
}

impl Session {
    fn send(&self, line: String) {
        let _ = self.tx.send(line);
    }

    fn principal(&self) -> Result<&Principal> {
        self.principal
            .as_ref()
            .ok_or_else(|| anyhow!("session has no principal"))
    }
}

#[cfg(test)]
mod tests {
    use super::{escape_tag_value, metadata_tags, Server, Session};
    use crate::protocol::Command;
    use anyhow::Result;
    use std::collections::HashSet;
    use tokio::sync::mpsc;

    fn test_server() -> Result<Server> {
        let db = crate::store::open(":memory:")?;
        Ok(Server::new(db))
    }

    #[test]
    fn role_words_in_a_project_channel_address_the_role_holders() -> Result<()> {
        let server = test_server()?;
        let (owner, _) = server
            .auth()
            .register_principal("boss", Some("leader"), None)?;
        let (reviewer, _) = server.auth().register_principal("rev", None, None)?;
        server.create_project_api(&owner, "#quest", "Quest", None)?;
        server.set_project_member_api(&owner, "#quest", "rev", Some("coordinator"))?;

        let hit = server.role_mentions("#quest", "ready for review @coordinator");
        assert!(hit.contains(&reviewer.id));
        assert!(hit.len() == 1, "the project leader is not a coordinator");
        assert!(server.role_mentions("#general", "@coordinator").is_empty());
        assert!(server.role_mentions("#quest", "no mention here").is_empty());
        Ok(())
    }

    #[test]
    fn a_sprint_room_acts_as_its_project_for_roles_and_the_project_manager() -> Result<()> {
        let server = test_server()?;
        let (owner, _) = server
            .auth()
            .register_principal("boss2", Some("leader"), None)?;
        let (manager, _) = server.auth().register_principal("pm", None, None)?;
        server.create_project_api(&owner, "#proj", "Proj", None)?;
        server.set_project_pm_api(&owner, "#proj", "pm")?;

        // Only a manager of the project opens a sprint.
        let (outsider, _) = server.auth().register_principal("visitor", None, None)?;
        assert!(server.open_room_api(&outsider, "#proj", None).is_err());

        let room = server.open_room_api(&owner, "#proj", Some("First playable"))?;
        assert_eq!(room.channel, "#proj-s1");
        assert_eq!(room.label, "s1");
        assert_eq!(server.project_of(&room.channel), "#proj");
        assert_eq!(server.project_of("#general"), "#general");
        // The PM is the PM in the room too, so it keeps a conversation per room.
        assert!(server.is_pm_of(&room.channel, &manager.id));
        assert!(server
            .role_mentions(&room.channel, "ready @coordinator")
            .contains(&manager.id));
        // Members of the project were put in the room; later members join it too.
        assert!(server.is_member(&manager.id, &room.channel)?);
        server.set_project_member_api(&owner, "#proj", "visitor", None)?;
        assert!(server.is_member(&outsider.id, &room.channel)?);

        let second = server.open_room_api(&manager, "#proj-s1", None)?;
        assert_eq!(second.channel, "#proj-s2", "opening from inside a room opens the project's next one");
        Ok(())
    }

    fn test_session(token: &str, nick: &str) -> Session {
        let (tx, _rx) = mpsc::unbounded_channel();
        let (shutdown_tx, _shutdown_rx) = mpsc::unbounded_channel();
        Session {
            session_id: format!("sess_{}", uuid::Uuid::new_v4().simple()),
            principal: None,
            token_info: None,
            nick: Some(nick.to_string()),
            pass_token: Some(token.to_string()),
            registered: false,
            capabilities: HashSet::new(),
            tx,
            shutdown_tx,
        }
    }

    fn test_session_with_rx(
        token: Option<&str>,
        nick: Option<&str>,
    ) -> (Session, mpsc::UnboundedReceiver<String>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let (shutdown_tx, _shutdown_rx) = mpsc::unbounded_channel();
        (
            Session {
                session_id: format!("sess_{}", uuid::Uuid::new_v4().simple()),
                principal: None,
                token_info: None,
                nick: nick.map(str::to_string),
                pass_token: token.map(str::to_string),
                registered: false,
                capabilities: HashSet::new(),
                tx,
                shutdown_tx,
            },
            rx,
        )
    }

    #[test]
    fn replaced_session_disconnect_does_not_mark_active_principal_disconnected() -> Result<()> {
        let server = test_server()?;
        let mut old_session = test_session("agent-a-token", "agent-a");
        let mut new_session = test_session("agent-a-token", "agent-a");

        server.try_register(&mut old_session)?;
        server.try_register(&mut new_session)?;
        server.disconnect(&old_session)?;

        let disconnected_at: Option<i64> =
            server.db.lock().expect("sqlite mutex poisoned").query_row(
                "SELECT disconnected_at FROM principals WHERE id = 'agent-a'",
                [],
                |row| row.get(0),
            )?;
        assert_eq!(disconnected_at, None);

        server.disconnect(&new_session)?;
        let disconnected_at: Option<i64> =
            server.db.lock().expect("sqlite mutex poisoned").query_row(
                "SELECT disconnected_at FROM principals WHERE id = 'agent-a'",
                [],
                |row| row.get(0),
            )?;
        assert!(disconnected_at.is_some());

        Ok(())
    }

    #[tokio::test]
    async fn reissue_invalidates_commands_and_preserves_new_session_during_old_cleanup(
    ) -> Result<()> {
        let server = test_server()?;
        let mut old = test_session("agent-a-token", "agent-a");
        let (shutdown_tx, mut shutdown_rx) = mpsc::unbounded_channel();
        old.shutdown_tx = shutdown_tx;
        server.try_register(&mut old)?;
        server.join(&old, "#kept".into())?;
        let first = server.reissue_token("agent-a")?;
        assert_eq!(
            shutdown_rx.try_recv()?.as_str(),
            "Credentials reissued by owner"
        );
        assert!(!server.inner.lock().unwrap().clients.contains_key("agent-a"));
        assert!(server
            .handle_command(
                &mut old,
                Command::Privmsg {
                    target: "#kept".into(),
                    body: "must not be stored".into(),
                }
            )
            .await
            .is_err());
        assert!(server
            .handle_command(&mut old, Command::Ping("stale".into()))
            .await
            .is_err());
        let mut fresh = test_session(&first, "agent-a");
        server.try_register(&mut fresh)?;
        assert!(fresh.registered);
        server.disconnect(&old)?;
        assert!(server.ensure_registered(&fresh)?);
        let disconnected_at: Option<i64> = server.db.lock().unwrap().query_row(
            "SELECT disconnected_at FROM principals WHERE id = 'agent-a'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(disconnected_at, None);
        assert!(server.is_member("agent-a", "#kept")?);
        let stored: i64 = server.db.lock().unwrap().query_row(
            "SELECT COUNT(*) FROM messages WHERE body = 'must not be stored'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(stored, 0);
        // A reset of a session using an already-reissued (hashed) token works too.
        let second = server.reissue_token("agent-a")?;
        assert!(server.ensure_registered(&fresh).is_err());
        server.disconnect(&fresh)?;
        let mut latest = test_session(&second, "agent-a");
        server.try_register(&mut latest)?;
        assert!(latest.registered);
        Ok(())
    }

    #[tokio::test]
    async fn reissue_failure_keeps_the_active_session_usable() -> Result<()> {
        let server = test_server()?;
        let mut session = test_session("agent-a-token", "agent-a");
        let (shutdown_tx, mut shutdown_rx) = mpsc::unbounded_channel();
        session.shutdown_tx = shutdown_tx;
        server.try_register(&mut session)?;
        server.db.lock().unwrap().execute_batch(
            "CREATE TEMP TRIGGER fail_reissue BEFORE INSERT ON principal_tokens
             BEGIN SELECT RAISE(ABORT, 'injected reissue failure'); END;",
        )?;
        assert!(server.reissue_token("agent-a").is_err());
        assert!(shutdown_rx.try_recv().is_err());
        assert!(server.ensure_registered(&session)?);
        assert_eq!(server.auth.authenticate("agent-a-token")?.id, "agent-a");
        assert!(
            !server
                .handle_command(&mut session, Command::Ping("still-active".into()))
                .await?
        );
        Ok(())
    }

    #[test]
    fn reissue_racing_old_token_registrations_leaves_no_stale_client() -> Result<()> {
        use std::sync::{Arc, Barrier};
        let server = test_server()?;
        let barrier = Arc::new(Barrier::new(9));
        std::thread::scope(|scope| -> Result<()> {
            let mut registrations = Vec::new();
            for _ in 0..8 {
                let server = server.clone();
                let barrier = barrier.clone();
                registrations.push(scope.spawn(move || -> Result<Session> {
                    let mut session = test_session("agent-a-token", "agent-a");
                    barrier.wait();
                    server.try_register(&mut session)?;
                    Ok(session)
                }));
            }
            barrier.wait();
            let token = server.reissue_token("agent-a")?;
            for registration in registrations {
                let session = registration.join().unwrap()?;
                if session.registered {
                    assert!(server.ensure_registered(&session).is_err());
                }
                server.disconnect(&session)?;
            }
            assert!(!server.inner.lock().unwrap().clients.contains_key("agent-a"));
            let mut fresh = test_session(&token, "agent-a");
            server.try_register(&mut fresh)?;
            assert!(fresh.registered);
            Ok(())
        })
    }

    #[tokio::test]
    async fn reissue_http_closes_idle_irc_socket_and_allows_only_the_new_token() -> Result<()> {
        use std::time::Duration;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::{TcpListener, TcpStream};
        use tokio::time::timeout;

        let server = test_server()?;
        let irc = TcpListener::bind("127.0.0.1:0").await?;
        let irc_addr = irc.local_addr()?;
        let irc_server = server.clone();
        let irc_task = tokio::spawn(async move {
            for _ in 0..3 {
                let (stream, _) = irc.accept().await.unwrap();
                let server = irc_server.clone();
                tokio::spawn(async move { server.handle_stream(stream).await });
            }
        });
        let http = TcpListener::bind("127.0.0.1:0").await?;
        let http_addr = http.local_addr()?;
        let mut info =
            crate::http::build_runtime_info(&irc_addr.to_string(), &http_addr.to_string(), None);
        info.register_key = Some("reissue-test-key".into());
        let app = crate::http::router(server.clone(), info);
        let http_task = tokio::spawn(async move { axum::serve(http, app).await });

        let (read, mut write) = TcpStream::connect(irc_addr).await?.into_split();
        let mut lines = BufReader::new(read).lines();
        write
            .write_all(b"PASS agent-a-token\r\nNICK agent-a\r\nUSER agent-a 0 * :test\r\n")
            .await?;
        loop {
            let line = timeout(Duration::from_secs(5), lines.next_line())
                .await??
                .unwrap();
            if line.contains(" 376 ") {
                break;
            }
        }
        let client = reqwest::Client::new();
        let url = format!("http://{http_addr}/v1/owner/reissue");
        // A rejected request must leave the socket alive.
        let denied = client
            .post(&url)
            .json(&serde_json::json!({
                "nick": "agent-a", "invite": "wrong",
            }))
            .send()
            .await?;
        assert_eq!(denied.status().as_u16(), 403);
        write.write_all(b"PING :before-reset\r\n").await?;
        loop {
            let line = timeout(Duration::from_secs(5), lines.next_line())
                .await??
                .unwrap();
            if line.contains(" PONG ") {
                break;
            }
        }
        let reset = client
            .post(&url)
            .json(&serde_json::json!({
                "nick": "agent-a", "invite": "reissue-test-key",
            }))
            .send()
            .await?;
        assert_eq!(reset.status().as_u16(), 200);
        let body: serde_json::Value = reset.json().await?;
        let token = body["token"].as_str().unwrap();
        // No input on the old connection is needed to trigger its shutdown.
        timeout(Duration::from_secs(5), async {
            while lines.next_line().await?.is_some() {}
            anyhow::Ok(())
        })
        .await??;
        assert!(!server.inner.lock().unwrap().clients.contains_key("agent-a"));

        let (read, mut write) = TcpStream::connect(irc_addr).await?.into_split();
        let mut stale = BufReader::new(read).lines();
        write
            .write_all(b"PASS agent-a-token\r\nNICK agent-a\r\n")
            .await?;
        loop {
            let line = timeout(Duration::from_secs(5), stale.next_line())
                .await??
                .unwrap();
            assert!(!line.contains(" 001 "));
            if line.contains(" 464 ") {
                break;
            }
        }
        drop(write);
        drop(stale);
        let (read, mut write) = TcpStream::connect(irc_addr).await?.into_split();
        let mut fresh = BufReader::new(read).lines();
        write
            .write_all(format!("PASS {token}\r\nNICK agent-a\r\n").as_bytes())
            .await?;
        loop {
            let line = timeout(Duration::from_secs(5), fresh.next_line())
                .await??
                .unwrap();
            if line.contains(" 001 ") {
                break;
            }
        }
        write.write_all(b"QUIT\r\n").await?;
        http_task.abort();
        irc_task.abort();
        Ok(())
    }

    #[test]
    fn new_channel_membership_starts_at_current_channel_max_seq() -> Result<()> {
        let server = test_server()?;
        let before_join = server
            .history
            .append("#existing", "agent-a", "before join")?;
        let mut session = test_session("agent-b-token", "agent-b");

        server.try_register(&mut session)?;
        server.join(&session, "#existing".to_string())?;

        let last_seen_seq: i64 = server.db.lock().expect("sqlite mutex poisoned").query_row(
            "SELECT last_seen_seq FROM channel_memberships WHERE principal_id = 'agent-b' AND channel = '#existing'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(last_seen_seq, before_join.seq);

        Ok(())
    }

    #[test]
    fn metadata_tags_use_ircv3_escaping() {
        assert_eq!(
            escape_tag_value("hello world;ok\\done\r\n"),
            "hello\\sworld\\:ok\\\\done\\r\\n"
        );
    }

    #[test]
    fn metadata_tags_include_replay_flag_when_requested() {
        assert_eq!(
            metadata_tags(42, "msg_abc", 1_776_250_000, true),
            "@seq=42;msg-id=msg_abc;time=1776250000;replay=1"
        );
    }

    #[test]
    fn irc_auth_failure_legacy_token_and_session_replacement_forward_security_audits(
    ) -> Result<()> {
        let syslog_rx = std::net::UdpSocket::bind("127.0.0.1:0")?;
        syslog_rx.set_nonblocking(true)?;
        let syslog_addr = syslog_rx.local_addr()?;

        let forwarder = crate::logging::EventForwarder::new(crate::logging::LogConfig {
            syslog_target: Some(crate::logging::SyslogTarget::Udp(syslog_addr.to_string())),
            syslog_format: crate::logging::SyslogFormat::Rfc5424,
            stderr_errors: false,
            ..crate::logging::LogConfig::default()
        })?;
        let db = crate::store::open(":memory:")?;
        let server = Server::with_logger(db, forwarder);

        let mut bad_session = test_session("wrong-irc-secret", "agent-a");
        server.try_register(&mut bad_session)?;
        assert!(!bad_session.registered);

        let mut first_session = test_session("agent-a-token", "agent-a");
        server.try_register(&mut first_session)?;
        assert!(first_session.registered);

        let mut replacement_session = test_session("agent-a-token", "agent-a");
        server.try_register(&mut replacement_session)?;
        assert!(replacement_session.registered);

        server.logger().flush();
        let mut received = Vec::new();
        let mut buf = [0u8; 4096];
        while let Ok((n, _)) = syslog_rx.recv_from(&mut buf) {
            received.push(String::from_utf8_lossy(&buf[..n]).into_owned());
        }
        let joined = received.join("\n");
        for expected in [
            "auth.irc_failed",
            "auth.legacy_token",
            "auth.irc_session_replaced",
        ] {
            assert!(
                received.iter().any(|msg| msg.contains(expected)),
                "missing IRC audit event '{expected}' in:\n{joined}"
            );
        }
        assert!(!joined.contains("wrong-irc-secret"));
        assert!(!joined.contains("agent-a-token"));
        Ok(())
    }

    #[tokio::test]
    async fn unregistered_command_returns_451_without_closing_session() -> Result<()> {
        let server = test_server()?;
        let (mut session, mut rx) = test_session_with_rx(None, None);

        let should_quit = server
            .handle_command(&mut session, Command::Join("#work".to_string()))
            .await?;
        assert!(!should_quit);

        let notice = rx.recv().await.expect("451 response");
        assert_eq!(notice, ":aircd 451 * :You have not registered");
        assert!(!session.registered);

        server
            .handle_command(&mut session, Command::Pass("human-token".to_string()))
            .await?;
        server
            .handle_command(&mut session, Command::Nick("human".to_string()))
            .await?;
        server
            .handle_command(&mut session, Command::User("human".to_string()))
            .await?;
        assert!(session.registered);

        let should_quit = server
            .handle_command(&mut session, Command::Join("#work".to_string()))
            .await?;
        assert!(!should_quit);

        Ok(())
    }

    #[tokio::test]
    async fn role_and_equipment_lock_commands_broadcast_and_persist() -> Result<()> {
        let server = test_server()?;
        let (mut lead, mut lead_rx) = test_session_with_rx(Some("human-token"), Some("human"));
        server.try_register(&mut lead)?;
        server
            .handle_command(&mut lead, Command::Join("#vr-lab".to_string()))
            .await?;

        while lead_rx.try_recv().is_ok() {}

        server
            .handle_command(
                &mut lead,
                Command::RoleSet {
                    nick: "agent-a".to_string(),
                    role: "leader".to_string(),
                },
            )
            .await?;
        let role_notice = lead_rx.recv().await.expect("role notice");
        assert!(role_notice.contains("ROLE @agent-a set to 'leader'"));

        server
            .handle_command(
                &mut lead,
                Command::LockAcquire {
                    resource: "quest-headset-1".to_string(),
                    channel: Some("#vr-lab".to_string()),
                    ttl_seconds: Some(600),
                    note: Some("capture run".to_string()),
                },
            )
            .await?;
        let lock_notice = lead_rx.recv().await.expect("lock notice");
        assert!(lock_notice.contains("LOCK quest-headset-1 acquired by @human"));

        // Second session cannot lock the same equipment while held.
        let (mut other, mut other_rx) =
            test_session_with_rx(Some("agent-b-token"), Some("agent-b"));
        server.try_register(&mut other)?;
        while other_rx.try_recv().is_ok() {}
        server
            .handle_command(
                &mut other,
                Command::LockAcquire {
                    resource: "quest-headset-1".to_string(),
                    channel: Some("#vr-lab".to_string()),
                    ttl_seconds: Some(600),
                    note: None,
                },
            )
            .await?;
        let err_notice = other_rx.recv().await.expect("conflict notice");
        assert!(err_notice.contains("failed"));

        // Owner releases cleanly.
        server
            .handle_command(
                &mut lead,
                Command::LockRelease("quest-headset-1".to_string()),
            )
            .await?;
        let unlock_notice = lead_rx.recv().await.expect("unlock notice");
        assert!(unlock_notice.contains("UNLOCK quest-headset-1 released"));

        Ok(())
    }

    #[tokio::test]
    async fn direct_message_privmsg_delivers_live_and_persists_dm_history() -> Result<()> {
        let server = test_server()?;
        let (mut a, _a_rx) = test_session_with_rx(Some("agent-a-token"), Some("agent-a"));
        let (mut b, mut b_rx) = test_session_with_rx(Some("agent-b-token"), Some("agent-b"));
        server.try_register(&mut a)?;
        server.try_register(&mut b)?;
        while b_rx.try_recv().is_ok() {}

        server
            .handle_command(
                &mut a,
                Command::Privmsg {
                    target: "agent-b".to_string(),
                    body: "grab the quest headset when ready".to_string(),
                },
            )
            .await?;

        let delivered = b_rx.recv().await.expect("dm delivered to agent-b");
        assert!(delivered.contains(":agent-a PRIVMSG agent-b :grab the quest headset when ready"));
        let dms = server
            .history()
            .list_dm_messages("agent-b", Some("agent-a"), 0, 10)?;
        assert_eq!(dms.len(), 1);
        assert_eq!(dms[0].body, "grab the quest headset when ready");
        Ok(())
    }

    #[tokio::test]
    async fn weechat_irc_join_mode_names_and_whois_numerics_work() -> Result<()> {
        let server = test_server()?;
        let (mut lead, mut lead_rx) = test_session_with_rx(Some("human-token"), Some("human"));
        server.try_register(&mut lead)?;
        while lead_rx.try_recv().is_ok() {}

        server
            .handle_command(&mut lead, Command::Join("#general,#vr-lab".to_string()))
            .await?;

        let mut seen_names = false;
        while let Ok(line) = lead_rx.try_recv() {
            if line.contains(" 353 human = #vr-lab :") {
                seen_names = true;
            }
        }
        assert!(seen_names, "JOIN must emit 353 NAMES for WeeChat nicklist");

        server
            .handle_command(
                &mut lead,
                Command::Mode {
                    target: "#vr-lab".to_string(),
                    flags: Some("+o".to_string()),
                    arg: Some("agent-a".to_string()),
                },
            )
            .await?;

        let mut seen_mode = false;
        while let Ok(line) = lead_rx.try_recv() {
            if line.contains("MODE #vr-lab +o agent-a") {
                seen_mode = true;
            }
        }
        assert!(seen_mode, "MODE +o must broadcast mode update");
        let updated = server
            .auth()
            .resolve_by_name("agent-a")?
            .expect("agent-a exists");
        assert_eq!(updated.role, "leader");

        server
            .handle_command(&mut lead, Command::Whois("agent-a".to_string()))
            .await?;
        let whois_line = lead_rx.recv().await.expect("311 WHOIS");
        assert!(whois_line.contains(" 311 human agent-a agent-a aircd * :role=leader"));
        Ok(())
    }

    #[tokio::test]
    async fn privmsg_does_not_echo_back_to_sender_unless_echo_message_enabled() -> Result<()> {
        let server = test_server()?;
        let (mut sender, mut sender_rx) = test_session_with_rx(Some("human-token"), Some("human"));
        let (mut peer, mut peer_rx) = test_session_with_rx(Some("agent-a-token"), Some("agent-a"));
        server.try_register(&mut sender)?;
        server.try_register(&mut peer)?;
        server
            .handle_command(&mut sender, Command::Join("#work".to_string()))
            .await?;
        server
            .handle_command(&mut peer, Command::Join("#work".to_string()))
            .await?;
        while sender_rx.try_recv().is_ok() {}
        while peer_rx.try_recv().is_ok() {}

        server
            .handle_command(
                &mut sender,
                Command::Privmsg {
                    target: "#work".to_string(),
                    body: "no double chat".to_string(),
                },
            )
            .await?;

        assert!(
            sender_rx.try_recv().is_err(),
            "standard IRC sender must not receive its own PRIVMSG echo"
        );
        let peer_line = peer_rx.recv().await.expect("peer receives message");
        assert!(peer_line.contains(":human PRIVMSG #work :no double chat"));

        // Reconnect sender and ensure their own sent message is not replayed back either.
        server.disconnect(&sender)?;
        let (mut reconnected, mut reconnected_rx) =
            test_session_with_rx(Some("human-token"), Some("human"));
        server.try_register(&mut reconnected)?;
        while let Ok(line) = reconnected_rx.try_recv() {
            assert!(
                !line.contains("no double chat"),
                "reconnecting sender must not replay their own message: {line}"
            );
        }

        // Enabling echo-message CAP opts into echoing back to the sender session.
        server
            .handle_command(
                &mut reconnected,
                Command::Cap {
                    subcommand: "REQ".to_string(),
                    capabilities: vec!["message-tags".to_string(), "echo-message".to_string()],
                },
            )
            .await?;
        while reconnected_rx.try_recv().is_ok() {}
        server
            .handle_command(
                &mut reconnected,
                Command::Privmsg {
                    target: "#work".to_string(),
                    body: "explicit echo".to_string(),
                },
            )
            .await?;
        let echo_line = reconnected_rx.recv().await.expect("echo-message echo");
        assert!(echo_line.contains(":human PRIVMSG #work :explicit echo"));
        Ok(())
    }

    #[tokio::test]
    async fn duplicate_join_on_autojoined_channel_does_not_spam_numerics() -> Result<()> {
        let server = test_server()?;
        let (mut lead, _lead_rx) = test_session_with_rx(Some("human-token"), Some("human"));
        server.try_register(&mut lead)?;
        server
            .handle_command(&mut lead, Command::Join("#general".to_string()))
            .await?;
        server.disconnect(&lead)?;

        // Reconnect: try_register restores membership and sends initial JOIN/NAMES.
        let (mut reconnected, mut reconnected_rx) =
            test_session_with_rx(Some("human-token"), Some("human"));
        server.try_register(&mut reconnected)?;
        while reconnected_rx.try_recv().is_ok() {}

        // Client autojoin sends JOIN #general right after connect; should not duplicate.
        server
            .handle_command(&mut reconnected, Command::Join("#general".to_string()))
            .await?;
        assert!(
            reconnected_rx.try_recv().is_err(),
            "re-joining already online channel should not emit duplicate JOIN/NAMES"
        );
        Ok(())
    }

    #[tokio::test]
    async fn failed_wake_is_reported_in_the_channel_once_per_quota_cooldown() -> Result<()> {
        let server = test_server()?;
        let (human, _) = server
            .auth()
            .register_principal("quota-human", None, None)?;
        let (agent, _) = server
            .auth()
            .register_principal("quota-agent", None, None)?;
        let (command, flag) = if cfg!(windows) {
            ("cmd", "/C")
        } else {
            ("sh", "-c")
        };
        server.wakes().upsert(
            &agent.id,
            crate::wake::WakeConfigInput {
                command: command.into(),
                args: vec![
                    flag.into(),
                    "echo 429 Too Many Requests: RESOURCE_EXHAUSTED 1>&2 && exit 1".into(),
                ],
                cooldown_seconds: Some(0),
                timeout_seconds: Some(10),
                ..Default::default()
            },
        )?;
        let notices = || -> Result<Vec<String>> {
            Ok(server
                .history()
                .after("#cams", 0, 50)?
                .into_iter()
                .filter(|m| m.body.starts_with("wake failed:"))
                .map(|m| m.body)
                .collect())
        };

        server.post_message(&human, "#cams", "@quota-agent please build")?;
        for _ in 0..200 {
            if !notices()?.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        let posted = notices()?;
        assert_eq!(posted.len(), 1, "{posted:?}");
        assert!(
            posted[0].starts_with("wake failed: @quota-agent is out of quota (exited with code 1: 429 Too Many Requests"),
            "{}",
            posted[0]
        );

        // Later mentions are held back without launching or posting again.
        server.post_message(&human, "#cams", "@quota-agent are you there?")?;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(notices()?.len(), 1);
        let runs = server.wakes().runs(Some(&agent.id), 10)?;
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].status, "skipped");
        assert_eq!(runs[1].status, "failed");
        Ok(())
    }

    #[tokio::test]
    async fn server_dm_wake_loop_is_suppressed_while_channel_and_human_wakes_work() -> Result<()> {
        let server = test_server()?;
        let (human, _) = server.auth().register_principal("dm-loop-human", None, None)?;
        let (agent_a, _) = server.auth().register_principal("dm-loop-a", None, None)?;
        let (agent_b, _) = server.auth().register_principal("dm-loop-b", None, None)?;

        let (command, args) = if cfg!(windows) {
            (
                "cmd".to_string(),
                vec!["/C".to_string(), "echo".to_string(), "ok".to_string()],
            )
        } else {
            ("echo".to_string(), vec!["ok".to_string()])
        };
        for id in [&agent_a.id, &agent_b.id] {
            server.wakes().upsert(
                id,
                crate::wake::WakeConfigInput {
                    command: command.clone(),
                    args: args.clone(),
                    cooldown_seconds: Some(0),
                    timeout_seconds: Some(10),
                    ..Default::default()
                },
            )?;
        }

        // Agent A sends DM to Agent B -> B wakes.
        server.post_message(&agent_a, "@dm-loop-b", "hello from a")?;
        for _ in 0..100 {
            let runs = server.wakes().runs(Some(&agent_b.id), 10)?;
            if runs.len() == 1 && runs[0].status == "ok" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert_eq!(server.wakes().runs(Some(&agent_b.id), 10)?[0].status, "ok");

        // Agent B replies via DM to Agent A -> suppressed (recorded as skipped).
        server.post_message(&agent_b, "@dm-loop-a", "reply from b")?;
        let a_runs = server.wakes().runs(Some(&agent_a.id), 10)?;
        assert_eq!(a_runs.len(), 1);
        assert_eq!(a_runs[0].status, "skipped");

        // Channel mention from Agent B to Agent A still wakes A.
        server.post_message(&agent_b, "#cams", "@dm-loop-a check channel")?;
        for _ in 0..100 {
            let runs = server.wakes().runs(Some(&agent_a.id), 10)?;
            if runs.iter().any(|r| r.status == "ok") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(server
            .wakes()
            .runs(Some(&agent_a.id), 10)?
            .iter()
            .any(|r| r.status == "ok"));

        // Human DM to Agent B resets B so B can wake A again.
        server.post_message(&human, "@dm-loop-b", "human check-in")?;
        for _ in 0..100 {
            let runs = server.wakes().runs(Some(&agent_b.id), 10)?;
            if runs.len() == 2 && runs.iter().all(|r| r.status == "ok") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        server.post_message(&agent_b, "@dm-loop-a", "fresh dm after human")?;
        for _ in 0..100 {
            let runs = server.wakes().runs(Some(&agent_a.id), 10)?;
            if runs.iter().filter(|r| r.status == "ok").count() == 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert_eq!(
            server
                .wakes()
                .runs(Some(&agent_a.id), 10)?
                .iter()
                .filter(|r| r.status == "ok")
                .count(),
            2
        );
        Ok(())
    }

    #[test]
    fn task_repo_validation_and_repo_path_stay_scoped_to_project() -> Result<()> {
        let server = test_server()?;
        let (owner, _) = server
            .auth()
            .register_principal("repo-owner", Some("leader"), None)?;
        server.create_project_api(&owner, "#one", "One", None)?;
        server.create_project_api(&owner, "#two", "Two", None)?;
        server.gits().upsert("repo-one", "#one", "/repos/one", None, Some("main"), None)?;
        server.gits().upsert("repo-two", "#two", "/repos/two", None, Some("main"), None)?;

        let err = server
            .create_task_full_api(
                &owner,
                crate::tasks::NewTask {
                    channel: "#one".to_string(),
                    title: "Cross-project repo".to_string(),
                    repo: Some("repo-two".to_string()),
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert!(err.to_string().contains("#two"));
        assert!(err.to_string().contains("#one"));

        let created = server.create_task_full_api(
            &owner,
            crate::tasks::NewTask {
                channel: "#one".to_string(),
                title: "Valid repo".to_string(),
                repo: Some("REPO-ONE".to_string()),
                ..Default::default()
            },
        )?;
        assert_eq!(created.repo.as_deref(), Some("repo-one"));
        assert_eq!(
            server.repo_path_for("#one", Some("repo-one")).as_deref(),
            Some("/repos/one")
        );
        assert_eq!(
            server.repo_path_for("#one", Some("repo-two")).as_deref(),
            Some("/repos/one"),
            "repo_path_for should not resolve a repo attached to another project"
        );
        Ok(())
    }

    #[tokio::test]
    async fn irc_commands_enforce_token_scopes_and_expiry() -> Result<()> {
        let server = test_server()?;
        let (leader, _) = server
            .auth()
            .register_principal("irc-leader", Some("leader"), None)?;
        let (peer, _) = server
            .auth()
            .register_principal("irc-peer", Some("builder"), None)?;
        server.create_project_api(&leader, "#ircops", "IRC Ops", None)?;

        let read_tok = server.auth().issue_token_with_options(
            &leader.id,
            Some("ro"),
            false,
            Some(&["read-only".to_string()]),
            Some(3600),
        )?;
        let write_tok = server.auth().issue_token_with_options(
            &leader.id,
            Some("rw"),
            false,
            Some(&["builder".to_string()]),
            Some(3600),
        )?;
        let pm_tok = server.auth().issue_token_with_options(
            &leader.id,
            Some("pm"),
            false,
            Some(&["pm".to_string()]),
            Some(3600),
        )?;
        let admin_tok = server.auth().issue_token_with_options(
            &leader.id,
            Some("adm"),
            false,
            Some(&["admin".to_string()]),
            Some(3600),
        )?;

        let drain = |rx: &mut mpsc::UnboundedReceiver<String>| -> Vec<String> {
            let mut out = Vec::new();
            while let Ok(line) = rx.try_recv() {
                out.push(line);
            }
            out
        };

        // 1. read-only token: allows LIST, WHO, CHANNELS; denies JOIN, PRIVMSG, TASK CREATE, ROLE.
        let (mut ro_session, mut ro_rx) =
            test_session_with_rx(Some(&read_tok.token), Some("irc-leader"));
        server.try_register(&mut ro_session)?;
        assert!(ro_session.registered);
        let _ = drain(&mut ro_rx);

        server.handle_command(&mut ro_session, Command::List(None)).await?;
        assert!(drain(&mut ro_rx).iter().any(|l| l.contains(" 323 ")));
        server.handle_command(&mut ro_session, Command::Who(None)).await?;
        assert!(drain(&mut ro_rx).iter().any(|l| l.contains(" 315 ")));
        server.handle_command(&mut ro_session, Command::Channels(None)).await?;
        assert!(drain(&mut ro_rx).iter().any(|l| l.contains(" 323 ")));

        server
            .handle_command(&mut ro_session, Command::Join("#ircops".into()))
            .await?;
        assert!(drain(&mut ro_rx).iter().any(|l| l.contains(" 481 ") && l.contains("write")));
        server
            .handle_command(
                &mut ro_session,
                Command::Privmsg {
                    target: "#ircops".into(),
                    body: "denied".into(),
                },
            )
            .await?;
        assert!(drain(&mut ro_rx).iter().any(|l| l.contains(" 481 ")));
        server
            .handle_command(
                &mut ro_session,
                Command::TaskCreate {
                    channel: "#ircops".into(),
                    title: "Denied".into(),
                },
            )
            .await?;
        assert!(drain(&mut ro_rx).iter().any(|l| l.contains(" 481 ") && l.contains("pm")));
        server.disconnect(&ro_session)?;

        // 2. builder (write) token: allows JOIN, PRIVMSG, TASK CLAIM; denies TASK CREATE, ROLE, force-UNLOCK.
        let pm_task = server.create_task_full_api(
            &leader,
            crate::tasks::NewTask {
                channel: "#ircops".to_string(),
                title: "Claimable".to_string(),
                ..Default::default()
            },
        )?;
        server
            .locks()
            .acquire("db-migration", &peer.id, "#ircops", None, Some(300))?;

        let (mut rw_session, mut rw_rx) =
            test_session_with_rx(Some(&write_tok.token), Some("irc-leader"));
        server.try_register(&mut rw_session)?;
        let _ = drain(&mut rw_rx);
        server
            .handle_command(&mut rw_session, Command::Join("#ircops-rw".into()))
            .await?;
        assert!(drain(&mut rw_rx).iter().any(|l| l.contains(" JOIN #ircops-rw")));
        server
            .handle_command(&mut rw_session, Command::TaskClaim(pm_task.id.clone()))
            .await?;
        assert!(drain(&mut rw_rx).iter().all(|l| !l.contains(" 481 ")));
        assert_eq!(
            server.tasks().get(&pm_task.id)?.unwrap().claimed_by.as_deref(),
            Some("irc-leader")
        );
        server
            .handle_command(
                &mut rw_session,
                Command::TaskCreate {
                    channel: "#ircops".into(),
                    title: "Need PM".into(),
                },
            )
            .await?;
        assert!(drain(&mut rw_rx).iter().any(|l| l.contains(" 481 ") && l.contains("pm")));
        server
            .handle_command(
                &mut rw_session,
                Command::LockRelease("db-migration".into()),
            )
            .await?;
        assert!(drain(&mut rw_rx).iter().any(|l| l.contains(" 481 ") && l.contains("admin")));
        server.disconnect(&rw_session)?;

        // 3. pm token: allows TASK CREATE; denies ROLE and force-UNLOCK.
        let (mut pm_session, mut pm_rx) =
            test_session_with_rx(Some(&pm_tok.token), Some("irc-leader"));
        server.try_register(&mut pm_session)?;
        let _ = drain(&mut pm_rx);
        server
            .handle_command(
                &mut pm_session,
                Command::TaskCreate {
                    channel: "#ircops".into(),
                    title: "Created by PM".into(),
                },
            )
            .await?;
        assert!(drain(&mut pm_rx).iter().all(|l| !l.contains(" 481 ")));
        server
            .handle_command(
                &mut pm_session,
                Command::RoleSet {
                    nick: "irc-peer".into(),
                    role: "reviewer".into(),
                },
            )
            .await?;
        assert!(drain(&mut pm_rx).iter().any(|l| l.contains(" 481 ") && l.contains("admin")));
        server.disconnect(&pm_session)?;

        // 4. admin token: allows ROLE and force-UNLOCK.
        let (mut adm_session, mut adm_rx) =
            test_session_with_rx(Some(&admin_tok.token), Some("irc-leader"));
        server.try_register(&mut adm_session)?;
        let _ = drain(&mut adm_rx);
        server
            .handle_command(
                &mut adm_session,
                Command::RoleSet {
                    nick: "irc-peer".into(),
                    role: "reviewer".into(),
                },
            )
            .await?;
        assert!(drain(&mut adm_rx).iter().all(|l| !l.contains(" 481 ")));
        server
            .handle_command(
                &mut adm_session,
                Command::LockRelease("db-migration".into()),
            )
            .await?;
        assert!(drain(&mut adm_rx).iter().all(|l| !l.contains(" 481 ")));
        assert!(server.locks().list(Some("#ircops"))?.is_empty());

        // 5. Expired token fails mid-session and on fresh registration.
        server.db.lock().expect("sqlite mutex poisoned").execute(
            "UPDATE principal_tokens SET expires_at = 1 WHERE token_id = ?1",
            rusqlite::params![admin_tok.token_id],
        )?;
        assert!(server.ensure_registered(&adm_session).is_err());

        let (mut exp_session, _exp_rx) =
            test_session_with_rx(Some(&admin_tok.token), Some("irc-leader"));
        server.try_register(&mut exp_session)?;
        assert!(!exp_session.registered);
        Ok(())
    }
}

fn role_prefix(role: &str) -> &'static str {
    match role {
        "leader" | "coordinator" | "op" | "admin" => "@",
        "reviewer" | "builder" | "tester" | "human" => "+",
        _ => "",
    }
}

fn normalize_channel(channel: &str) -> String {
    let channel = channel.trim();
    if channel.starts_with('#') {
        channel.to_string()
    } else if channel.is_empty() {
        "#demo".to_string()
    } else {
        format!("#{channel}")
    }
}

fn metadata_tags(seq: i64, msg_id: &str, created_at: i64, replay: bool) -> String {
    let mut tags = vec![
        ("seq", seq.to_string()),
        ("msg-id", msg_id.to_string()),
        ("time", created_at.to_string()),
    ];
    if replay {
        tags.push(("replay", "1".to_string()));
    }
    format_message_tags(&tags)
}

fn format_message_tags(tags: &[(&str, String)]) -> String {
    format!(
        "@{}",
        tags.iter()
            .map(|(key, value)| format!("{key}={}", escape_tag_value(value)))
            .collect::<Vec<_>>()
            .join(";")
    )
}

fn task_failure_tags(action: &str, task_id: &str, actor: &str) -> String {
    format_message_tags(&[
        ("task-id", task_id.to_string()),
        ("task-action", action.to_string()),
        ("task-status", "failed".to_string()),
        ("task-actor", actor.to_string()),
    ])
}

fn escape_tag_value(value: &str) -> String {
    let mut escaped = String::new();
    for character in value.chars() {
        match character {
            ';' => escaped.push_str("\\:"),
            ' ' => escaped.push_str("\\s"),
            '\\' => escaped.push_str("\\\\"),
            '\r' => escaped.push_str("\\r"),
            '\n' => escaped.push_str("\\n"),
            character => escaped.push(character),
        }
    }
    escaped
}
