//! Server error and security audit log forwarding to Windows Application Event Log and syslog.

use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeSet, VecDeque};
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs, UdpSocket};
#[cfg(unix)]
use std::os::unix::net::{UnixDatagram, UnixStream};
#[cfg(unix)]
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;

use crate::metrics::MetricsRegistry;

const QUEUE_CAPACITY: usize = 1024;
pub const AUDIT_STREAM_RING_CAPACITY: usize = 2048;
const AUDIT_BROADCAST_CAPACITY: usize = 2048;
const SYSLOG_IO_TIMEOUT: Duration = Duration::from_secs(2);
const FLUSH_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_EVENT_MESSAGE_CHARS: usize = 8192;
const DEFAULT_EVENT_SOURCE: &str = "aircd";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogCategory {
    ServerError,
    SecurityAudit,
    TaskState,
}

impl LogCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ServerError => "server_error",
            Self::SecurityAudit => "security_audit",
            Self::TaskState => "task_state",
        }
    }

    pub fn windows_category_id(self) -> u16 {
        match self {
            Self::ServerError => 1,
            Self::SecurityAudit => 2,
            Self::TaskState => 3,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Info,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Info => "info",
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "error" | "err" => Ok(Self::Error),
            "warning" | "warn" => Ok(Self::Warning),
            "info" => Ok(Self::Info),
            other => bail!("unsupported severity '{other}': expected info, warning, or error"),
        }
    }

    /// RFC 5424 severity code.
    pub fn syslog_code(self) -> u8 {
        match self {
            Self::Error => 3,
            Self::Warning => 4,
            Self::Info => 6,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SyslogFormat {
    #[default]
    Rfc5424,
    Rfc3164,
}

impl SyslogFormat {
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "rfc5424" | "5424" => Ok(Self::Rfc5424),
            "rfc3164" | "3164" => Ok(Self::Rfc3164),
            other => bail!("unsupported syslog_format '{other}': expected rfc5424 or rfc3164"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyslogFacility {
    Kern = 0,
    User = 1,
    Mail = 2,
    Daemon = 3,
    Auth = 4,
    Syslog = 5,
    Lpr = 6,
    News = 7,
    Uucp = 8,
    Cron = 9,
    Authpriv = 10,
    Ftp = 11,
    Local0 = 16,
    Local1 = 17,
    Local2 = 18,
    Local3 = 19,
    Local4 = 20,
    Local5 = 21,
    Local6 = 22,
    Local7 = 23,
}

impl SyslogFacility {
    pub fn code(self) -> u8 {
        self as u8
    }

    pub fn parse_optional(raw: &str) -> Result<Option<Self>> {
        let facility = match raw.trim().to_ascii_lowercase().as_str() {
            "auto" => return Ok(None),
            "kern" => Self::Kern,
            "user" => Self::User,
            "mail" => Self::Mail,
            "daemon" => Self::Daemon,
            "auth" => Self::Auth,
            "syslog" => Self::Syslog,
            "lpr" => Self::Lpr,
            "news" => Self::News,
            "uucp" => Self::Uucp,
            "cron" => Self::Cron,
            "authpriv" => Self::Authpriv,
            "ftp" => Self::Ftp,
            "local0" => Self::Local0,
            "local1" => Self::Local1,
            "local2" => Self::Local2,
            "local3" => Self::Local3,
            "local4" => Self::Local4,
            "local5" => Self::Local5,
            "local6" => Self::Local6,
            "local7" => Self::Local7,
            other => bail!(
                "unsupported syslog_facility '{other}': expected auto, daemon, auth, authpriv, user, syslog, or local0..local7"
            ),
        };
        Ok(Some(facility))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyslogTarget {
    Udp(String),
    Tcp(String),
    #[cfg(unix)]
    Unix(PathBuf),
}

impl SyslogTarget {
    pub fn parse(raw: &str) -> Result<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            bail!("syslog_target must not be empty");
        }
        if let Some(addr) = trimmed.strip_prefix("udp://") {
            validate_host_port(addr)?;
            return Ok(Self::Udp(addr.to_string()));
        }
        if let Some(addr) = trimmed.strip_prefix("tcp://") {
            validate_host_port(addr)?;
            return Ok(Self::Tcp(addr.to_string()));
        }
        if let Some(path) = trimmed.strip_prefix("unix://") {
            return parse_unix_target(path);
        }
        if trimmed.contains("://") {
            bail!("unsupported syslog_target scheme in '{trimmed}': expected udp://, tcp://, or unix://");
        }
        if trimmed.starts_with('/') {
            return parse_unix_target(trimmed);
        }
        validate_host_port(trimmed)?;
        Ok(Self::Udp(trimmed.to_string()))
    }
}

fn parse_unix_target(path: &str) -> Result<SyslogTarget> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        bail!("unix syslog socket path must not be empty");
    }
    #[cfg(unix)]
    {
        Ok(SyslogTarget::Unix(PathBuf::from(trimmed)))
    }
    #[cfg(not(unix))]
    {
        let _ = trimmed;
        bail!("unix:// syslog targets are only supported on Unix; use udp://host:port or tcp://host:port on Windows");
    }
}

fn validate_host_port(addr: &str) -> Result<()> {
    let trimmed = addr.trim();
    if trimmed.is_empty() || trimmed.chars().any(char::is_whitespace) {
        bail!("syslog host:port '{addr}' must not be empty or contain whitespace");
    }
    let Some((host, port_str)) = trimmed.rsplit_once(':') else {
        bail!("syslog target '{addr}' must include a port (host:port)");
    };
    if host.is_empty() || (host.starts_with('[') && !host.ends_with(']')) {
        bail!("syslog target '{addr}' has an invalid host");
    }
    let port: u16 = port_str
        .parse()
        .with_context(|| format!("invalid syslog port in '{addr}'"))?;
    if port == 0 {
        bail!("syslog port in '{addr}' must be non-zero");
    }
    Ok(())
}

pub fn validate_event_log_source(source: &str) -> Result<String> {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        bail!("server setting windows_event_log_source must not be empty");
    }
    if trimmed.contains('\0') || trimmed.contains('\\') || trimmed.contains('/') {
        bail!("windows_event_log_source must not contain path separators or NUL bytes");
    }
    Ok(trimmed.to_string())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogConfig {
    pub windows_event_log: bool,
    pub windows_event_log_source: String,
    pub syslog_target: Option<SyslogTarget>,
    pub syslog_format: SyslogFormat,
    pub syslog_facility: Option<SyslogFacility>,
    pub stderr_errors: bool,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            windows_event_log: false,
            windows_event_log_source: DEFAULT_EVENT_SOURCE.to_string(),
            syslog_target: None,
            syslog_format: SyslogFormat::Rfc5424,
            syslog_facility: None,
            stderr_errors: true,
        }
    }
}

impl LogConfig {
    pub fn has_forwarders(&self) -> bool {
        self.windows_event_log || self.syslog_target.is_some()
    }
}

#[derive(Clone, Debug)]
pub struct LogEvent {
    pub category: LogCategory,
    pub severity: Severity,
    pub action: String,
    pub outcome: Option<String>,
    pub actor: Option<String>,
    pub target: Option<String>,
    pub channel: Option<String>,
    pub task_id: Option<String>,
    pub from_status: Option<String>,
    pub to_status: Option<String>,
    pub details: Option<Value>,
    pub message: String,
    pub timestamp_secs: u64,
}

impl LogEvent {
    pub fn server_error(action: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            category: LogCategory::ServerError,
            severity: Severity::Error,
            action: action.into(),
            outcome: None,
            actor: None,
            target: None,
            channel: None,
            task_id: None,
            from_status: None,
            to_status: None,
            details: None,
            message: message.into(),
            timestamp_secs: unix_now_secs(),
        }
    }

    pub fn security_audit(
        severity: Severity,
        action: impl Into<String>,
        outcome: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            category: LogCategory::SecurityAudit,
            severity,
            action: action.into(),
            outcome: Some(outcome.into()),
            actor: None,
            target: None,
            channel: None,
            task_id: None,
            from_status: None,
            to_status: None,
            details: None,
            message: message.into(),
            timestamp_secs: unix_now_secs(),
        }
    }

    pub fn task_state(
        severity: Severity,
        action: impl Into<String>,
        outcome: impl Into<String>,
        task_id: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        let task_id_str = task_id.into();
        let target = (!task_id_str.trim().is_empty()).then(|| task_id_str.clone());
        Self {
            category: LogCategory::TaskState,
            severity,
            action: action.into(),
            outcome: Some(outcome.into()),
            actor: None,
            target,
            channel: None,
            task_id: (!task_id_str.trim().is_empty()).then_some(task_id_str),
            from_status: None,
            to_status: None,
            details: None,
            message: message.into(),
            timestamp_secs: unix_now_secs(),
        }
    }

    pub fn with_actor(mut self, actor: impl Into<String>) -> Self {
        let value = actor.into();
        if !value.trim().is_empty() {
            self.actor = Some(value);
        }
        self
    }

    pub fn with_target(mut self, target: impl Into<String>) -> Self {
        let value = target.into();
        if !value.trim().is_empty() {
            self.target = Some(value);
        }
        self
    }

    pub fn with_channel(mut self, channel: impl Into<String>) -> Self {
        let value = channel.into();
        if !value.trim().is_empty() {
            self.channel = Some(value);
        }
        self
    }

    pub fn with_transition(mut self, from_status: Option<&str>, to_status: Option<&str>) -> Self {
        self.from_status = from_status
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned);
        self.to_status = to_status
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned);
        self
    }

    pub fn with_details(mut self, details: Value) -> Self {
        if !details.is_null() {
            self.details = Some(details);
        }
        self
    }

    pub fn windows_event_id(&self) -> u32 {
        match self.action.as_str() {
            "server.startup_error" => 1001,
            "irc.accept_error" | "irc.connection_error" => 1002,
            "irc.tls_handshake_failed"
            | "irc.tls_connection_error"
            | "http.tls_handshake_failed"
            | "http.tls_connection_error" => 1003,
            "http.internal_error" => 1004,
            "webhook.enqueue_error" | "webhook.worker_error" => 1005,
            "wake.request_error" | "wake.run_error" | "wake.failure_notice_error" => 1006,
            _ if self.category == LogCategory::ServerError => 1000,
            "server.started" => 2000,
            "auth.http_failed" | "auth.irc_failed" => 2001,
            "auth.legacy_token" => 2002,
            "auth.register_denied"
            | "owner.auth_denied"
            | "notes.access_denied"
            | "worker.spawn_denied" => 2003,
            "auth.register_succeeded" => 2004,
            "token.reissued" | "token.rotated" => 2005,
            "account.created" | "worker.spawned" => 2006,
            "role.changed" | "project.pm_changed" | "project.member_changed" => 2007,
            "git.webhook_denied" | "webhook.updated" | "webhook.deleted" => 2008,
            "wake.config_changed"
            | "wake.deleted"
            | "template.upserted"
            | "template.deleted" => 2009,
            "auth.irc_session_replaced" => 2010,
            "http.rate_limited" => 2011,
            "irc.tls_client_verified" | "http.tls_client_verified" => 2012,
            _ if self.category == LogCategory::TaskState => 3000,
            _ => 2099,
        }
    }

    pub fn format_human(&self) -> String {
        let mut meta = vec![
            format!("action={}", sanitize_inline(&self.action)),
            format!("severity={}", self.severity.as_str()),
        ];
        if let Some(outcome) = &self.outcome {
            meta.push(format!("outcome={}", sanitize_inline(outcome)));
        }
        if let Some(actor) = &self.actor {
            meta.push(format!("actor={}", sanitize_inline(actor)));
        }
        if let Some(target) = &self.target {
            meta.push(format!("target={}", sanitize_inline(target)));
        }
        if let Some(channel) = &self.channel {
            meta.push(format!("channel={}", sanitize_inline(channel)));
        }
        if let Some(from) = &self.from_status {
            meta.push(format!("from_status={}", sanitize_inline(from)));
        }
        if let Some(to) = &self.to_status {
            meta.push(format!("to_status={}", sanitize_inline(to)));
        }
        let msg = truncate_chars(&sanitize_inline(&self.message), MAX_EVENT_MESSAGE_CHARS);
        format!("[{}] {} | {}", self.category.as_str(), meta.join(" "), msg)
    }
}

/// Structured JSON audit event streamed on `GET /v1/audit/stream`.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct AuditEvent {
    pub seq: u64,
    pub timestamp: u64,
    pub timestamp_iso: String,
    pub category: String,
    pub severity: String,
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    pub message: String,
}

impl AuditEvent {
    pub fn from_log_event(seq: u64, event: &LogEvent) -> Self {
        let category = classify_audit_category(event.category, &event.action);
        Self {
            seq,
            timestamp: event.timestamp_secs,
            timestamp_iso: format_rfc3339_utc(event.timestamp_secs),
            category: category.to_string(),
            severity: event.severity.as_str().to_string(),
            action: event.action.clone(),
            outcome: event.outcome.clone(),
            actor: event.actor.clone(),
            target: event.target.clone(),
            channel: event.channel.clone(),
            task_id: event.task_id.clone(),
            from_status: event.from_status.clone(),
            to_status: event.to_status.clone(),
            details: event.details.clone(),
            message: truncate_chars(&sanitize_inline(&event.message), MAX_EVENT_MESSAGE_CHARS),
        }
    }
}

fn is_auth_action(action: &str) -> bool {
    action.starts_with("auth.")
        || action.starts_with("token.")
        || action == "account.created"
        || action == "owner.auth_denied"
}

fn classify_audit_category(category: LogCategory, action: &str) -> &'static str {
    match category {
        LogCategory::TaskState => "task_state",
        LogCategory::ServerError => "server_error",
        LogCategory::SecurityAudit => {
            if is_auth_action(action) {
                "auth"
            } else {
                "security"
            }
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct AuditStreamFilter {
    pub categories: BTreeSet<String>,
    pub severities: BTreeSet<String>,
    pub actions: Vec<String>,
    pub actor: Option<String>,
    pub channel: Option<String>,
    pub task_id: Option<String>,
}

impl AuditStreamFilter {
    pub fn parse_categories(&mut self, raw: &str) -> Result<()> {
        for part in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            match part.to_ascii_lowercase().as_str() {
                "auth" => {
                    self.categories.insert("auth".to_string());
                }
                "security" => {
                    self.categories.insert("security".to_string());
                }
                "security_audit" => {
                    self.categories.insert("security".to_string());
                    self.categories.insert("auth".to_string());
                }
                "task_state" | "task-state" | "task" | "tasks" => {
                    self.categories.insert("task_state".to_string());
                }
                "server_error" | "server-error" | "error" | "errors" => {
                    self.categories.insert("server_error".to_string());
                }
                other => {
                    bail!(
                        "unsupported audit category '{other}': expected auth, security, task_state, server_error, or security_audit"
                    );
                }
            }
        }
        Ok(())
    }

    pub fn parse_severities(&mut self, raw: &str) -> Result<()> {
        for part in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let sev = Severity::parse(part)?;
            self.severities.insert(sev.as_str().to_string());
        }
        Ok(())
    }

    pub fn parse_actions(&mut self, raw: &str) {
        for part in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            self.actions.push(part.to_ascii_lowercase());
        }
    }

    pub fn set_actor(&mut self, raw: &str) {
        let clean = raw.trim().trim_start_matches('@');
        if !clean.is_empty() {
            self.actor = Some(clean.to_string());
        }
    }

    pub fn set_channel(&mut self, raw: &str) {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return;
        }
        if trimmed.starts_with('/') || trimmed.starts_with('#') || trimmed == "irc" {
            self.channel = Some(trimmed.to_string());
        } else {
            self.channel = Some(format!("#{trimmed}"));
        }
    }

    pub fn set_task_id(&mut self, raw: &str) {
        let clean = raw.trim();
        if !clean.is_empty() {
            self.task_id = Some(clean.to_string());
        }
    }

    pub fn matches(&self, event: &AuditEvent) -> bool {
        if !self.categories.is_empty() && !self.categories.contains(&event.category) {
            return false;
        }
        if !self.severities.is_empty() && !self.severities.contains(&event.severity) {
            return false;
        }
        if !self.actions.is_empty() {
            let act_lower = event.action.to_ascii_lowercase();
            let matched = self.actions.iter().any(|pattern| {
                if let Some(prefix) = pattern.strip_suffix('*') {
                    act_lower.starts_with(prefix)
                } else if pattern.ends_with('.') {
                    act_lower.starts_with(pattern)
                } else {
                    act_lower == *pattern
                }
            });
            if !matched {
                return false;
            }
        }
        if let Some(actor) = &self.actor {
            let ev_actor = event
                .actor
                .as_deref()
                .map(|a| a.trim_start_matches('@'))
                .unwrap_or("");
            if !ev_actor.eq_ignore_ascii_case(actor) {
                return false;
            }
        }
        if let Some(channel) = &self.channel {
            if event.channel.as_deref() != Some(channel.as_str()) {
                return false;
            }
        }
        if let Some(task_id) = &self.task_id {
            if event.task_id.as_deref() != Some(task_id.as_str()) {
                return false;
            }
        }
        true
    }
}

struct AuditRing {
    next_seq: u64,
    ring: VecDeque<AuditEvent>,
}

struct AuditHub {
    ring: Mutex<AuditRing>,
    tx: broadcast::Sender<AuditEvent>,
    metrics: OnceLock<Arc<MetricsRegistry>>,
}

impl AuditHub {
    fn new() -> Self {
        let (tx, _) = broadcast::channel(AUDIT_BROADCAST_CAPACITY);
        Self {
            ring: Mutex::new(AuditRing {
                next_seq: 1,
                ring: VecDeque::with_capacity(AUDIT_STREAM_RING_CAPACITY),
            }),
            tx,
            metrics: OnceLock::new(),
        }
    }

    fn attach_metrics(&self, metrics: Arc<MetricsRegistry>) {
        let _ = self.metrics.set(metrics);
    }

    fn publish(&self, event: &LogEvent) -> AuditEvent {
        let mut guard = self.ring.lock().expect("audit stream ring mutex poisoned");
        let seq = guard.next_seq;
        guard.next_seq = guard.next_seq.saturating_add(1);
        let audit_event = AuditEvent::from_log_event(seq, event);
        if guard.ring.len() >= AUDIT_STREAM_RING_CAPACITY {
            guard.ring.pop_front();
        }
        guard.ring.push_back(audit_event.clone());
        let _ = self.tx.send(audit_event.clone());
        drop(guard);
        if let Some(metrics) = self.metrics.get() {
            metrics.record_audit_event(&audit_event.category);
        }
        audit_event
    }

    fn subscribe(
        &self,
        since_seq: Option<u64>,
        replay: Option<usize>,
    ) -> (Vec<AuditEvent>, broadcast::Receiver<AuditEvent>, u64) {
        let guard = self.ring.lock().expect("audit stream ring mutex poisoned");
        let rx = self.tx.subscribe();
        let current_seq = guard.next_seq.saturating_sub(1);
        let mut initial = if let Some(since) = since_seq {
            let mut filtered: Vec<AuditEvent> = guard
                .ring
                .iter()
                .filter(|ev| ev.seq > since)
                .cloned()
                .collect();
            if let Some(limit) = replay {
                let keep = limit.min(AUDIT_STREAM_RING_CAPACITY);
                if filtered.len() > keep {
                    filtered = filtered.split_off(filtered.len() - keep);
                }
            }
            filtered
        } else if let Some(limit) = replay {
            let keep = limit.min(AUDIT_STREAM_RING_CAPACITY);
            let skip = guard.ring.len().saturating_sub(keep);
            guard.ring.iter().skip(skip).cloned().collect()
        } else {
            Vec::new()
        };
        initial.shrink_to_fit();
        (initial, rx, current_seq)
    }

    fn current_seq(&self) -> u64 {
        let guard = self.ring.lock().expect("audit stream ring mutex poisoned");
        guard.next_seq.saturating_sub(1)
    }
}

enum WorkerCommand {
    Event(LogEvent),
    Flush(mpsc::SyncSender<()>),
    Shutdown,
}

#[derive(Clone)]
pub struct EventForwarder {
    inner: Arc<ForwarderInner>,
}

struct ForwarderInner {
    config: LogConfig,
    tx: Option<SyncSender<WorkerCommand>>,
    worker: Mutex<Option<JoinHandle<()>>>,
    queue_full_warned: AtomicBool,
    audit: AuditHub,
}

impl Default for EventForwarder {
    fn default() -> Self {
        Self::new(LogConfig::default()).expect("default log config must be valid")
    }
}

impl EventForwarder {
    pub fn new(config: LogConfig) -> Result<Self> {
        let source = validate_event_log_source(&config.windows_event_log_source)?;
        let mut config = config;
        config.windows_event_log_source = source;

        #[cfg(not(windows))]
        if config.windows_event_log {
            eprintln!(
                "warning: windows_event_log is enabled in configuration, but Windows Event Log is only available on Windows"
            );
        }

        let (tx, worker) = if config.has_forwarders() {
            let (tx, rx) = mpsc::sync_channel(QUEUE_CAPACITY);
            let worker_config = config.clone();
            let handle = std::thread::Builder::new()
                .name("aircd-log-forwarder".to_string())
                .spawn(move || run_forwarder_worker(worker_config, rx))
                .context("spawn log forwarder worker thread")?;
            (Some(tx), Some(handle))
        } else {
            (None, None)
        };

        Ok(Self {
            inner: Arc::new(ForwarderInner {
                config,
                tx,
                worker: Mutex::new(worker),
                queue_full_warned: AtomicBool::new(false),
                audit: AuditHub::new(),
            }),
        })
    }

    #[allow(dead_code)]
    pub fn config(&self) -> &LogConfig {
        &self.inner.config
    }

    pub fn attach_metrics(&self, metrics: Arc<MetricsRegistry>) {
        self.inner.audit.attach_metrics(metrics);
    }

    pub fn audit_subscribe(
        &self,
        since_seq: Option<u64>,
        replay: Option<usize>,
    ) -> (Vec<AuditEvent>, broadcast::Receiver<AuditEvent>, u64) {
        self.inner.audit.subscribe(since_seq, replay)
    }

    #[allow(dead_code)]
    pub fn audit_current_seq(&self) -> u64 {
        self.inner.audit.current_seq()
    }

    pub fn log(&self, event: LogEvent) {
        self.inner.audit.publish(&event);
        if self.inner.config.stderr_errors && event.category == LogCategory::ServerError {
            eprintln!("{}", event.message);
        }
        if event.category == LogCategory::TaskState {
            return;
        }
        let Some(tx) = &self.inner.tx else {
            return;
        };
        match tx.try_send(WorkerCommand::Event(event)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                if !self.inner.queue_full_warned.swap(true, Ordering::Relaxed) {
                    eprintln!("log forwarder queue full; dropping excess forwarded log events");
                }
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    pub fn log_error(&self, action: &str, message: impl Into<String>) {
        self.log(LogEvent::server_error(action, message));
    }

    #[allow(clippy::too_many_arguments)]
    pub fn log_security(
        &self,
        severity: Severity,
        action: &str,
        outcome: &str,
        actor: Option<&str>,
        target: Option<&str>,
        channel: Option<&str>,
        message: impl Into<String>,
    ) {
        let mut event = LogEvent::security_audit(severity, action, outcome, message);
        if let Some(actor) = actor {
            event = event.with_actor(actor);
        }
        if let Some(target) = target {
            event = event.with_target(target);
        }
        if let Some(channel) = channel {
            event = event.with_channel(channel);
        }
        self.log(event);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn log_task_state(
        &self,
        severity: Severity,
        action: &str,
        task_id: &str,
        actor: Option<&str>,
        channel: Option<&str>,
        from_status: Option<&str>,
        to_status: Option<&str>,
        details: Option<Value>,
        message: impl Into<String>,
    ) {
        let mut event = LogEvent::task_state(severity, action, "success", task_id, message)
            .with_transition(from_status, to_status);
        if let Some(actor) = actor {
            event = event.with_actor(actor);
        }
        if let Some(channel) = channel {
            event = event.with_channel(channel);
        }
        if let Some(details) = details {
            event = event.with_details(details);
        }
        self.log(event);
    }

    /// Block until all previously queued events have been dispatched to the configured sinks.
    pub fn flush(&self) {
        let Some(tx) = &self.inner.tx else {
            return;
        };
        let (ack_tx, ack_rx) = mpsc::sync_channel(1);
        if tx.send(WorkerCommand::Flush(ack_tx)).is_ok() {
            let _ = ack_rx.recv_timeout(FLUSH_TIMEOUT);
        }
    }
}

impl Drop for ForwarderInner {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(WorkerCommand::Shutdown);
        }
        if let Ok(mut guard) = self.worker.lock() {
            if let Some(handle) = guard.take() {
                let _ = handle.join();
            }
        }
    }
}

fn run_forwarder_worker(config: LogConfig, rx: mpsc::Receiver<WorkerCommand>) {
    let mut sinks = ForwardSinks::open(&config);
    while let Ok(cmd) = rx.recv() {
        match cmd {
            WorkerCommand::Event(event) => sinks.dispatch(&event),
            WorkerCommand::Flush(ack) => {
                let _ = ack.send(());
            }
            WorkerCommand::Shutdown => break,
        }
    }
}

struct ForwardSinks {
    config: LogConfig,
    hostname: String,
    pid: u32,
    #[cfg(windows)]
    win_event_log: Option<win_event_log::EventSource>,
    udp_socket: Option<UdpSocket>,
    tcp_stream: Option<TcpStream>,
    #[cfg(unix)]
    unix_datagram: Option<UnixDatagram>,
    #[cfg(unix)]
    unix_stream: Option<UnixStream>,
    warned_windows: bool,
    warned_syslog: bool,
}

impl ForwardSinks {
    fn open(config: &LogConfig) -> Self {
        #[cfg(windows)]
        let mut warned_windows = false;
        #[cfg(not(windows))]
        let warned_windows = false;

        #[cfg(windows)]
        let win_event_log = if config.windows_event_log {
            match win_event_log::EventSource::register(&config.windows_event_log_source) {
                Ok(source) => Some(source),
                Err(err) => {
                    eprintln!("windows event log open failed: {err:#}");
                    warned_windows = true;
                    None
                }
            }
        } else {
            None
        };

        Self {
            config: config.clone(),
            hostname: detect_hostname(),
            pid: std::process::id(),
            #[cfg(windows)]
            win_event_log,
            udp_socket: None,
            tcp_stream: None,
            #[cfg(unix)]
            unix_datagram: None,
            #[cfg(unix)]
            unix_stream: None,
            warned_windows,
            warned_syslog: false,
        }
    }

    fn dispatch(&mut self, event: &LogEvent) {
        #[cfg(windows)]
        if self.config.windows_event_log {
            if self.win_event_log.is_none() {
                if let Ok(source) =
                    win_event_log::EventSource::register(&self.config.windows_event_log_source)
                {
                    self.win_event_log = Some(source);
                }
            }
            if let Some(source) = &self.win_event_log {
                let text = event.format_human();
                if let Err(err) = source.report(
                    event.severity,
                    event.category.windows_category_id(),
                    event.windows_event_id(),
                    &text,
                ) {
                    if !self.warned_windows {
                        eprintln!("windows event log write failed: {err:#}");
                        self.warned_windows = true;
                    }
                }
            }
        }

        if let Some(target) = self.config.syslog_target.clone() {
            let line = format_syslog_message(
                event,
                self.config.syslog_format,
                self.config.syslog_facility,
                &self.hostname,
                self.pid,
            );
            if let Err(err) = self.send_syslog(&target, &line) {
                if !self.warned_syslog {
                    eprintln!("syslog forward failed: {err:#}");
                    self.warned_syslog = true;
                }
            } else {
                self.warned_syslog = false;
            }
        }
    }

    fn send_syslog(&mut self, target: &SyslogTarget, line: &str) -> Result<()> {
        match target {
            SyslogTarget::Udp(addr) => {
                let resolved = addr
                    .to_socket_addrs()
                    .with_context(|| format!("resolve UDP syslog target '{addr}'"))?
                    .next()
                    .ok_or_else(|| anyhow!("no addresses resolved for UDP syslog '{addr}'"))?;
                let sock = match &self.udp_socket {
                    Some(sock) => sock,
                    None => {
                        let bind_addr = if resolved.is_ipv6() {
                            "[::]:0"
                        } else {
                            "0.0.0.0:0"
                        };
                        let sock = UdpSocket::bind(bind_addr).context("bind UDP syslog socket")?;
                        let _ = sock.set_write_timeout(Some(SYSLOG_IO_TIMEOUT));
                        self.udp_socket = Some(sock);
                        self.udp_socket.as_ref().expect("socket just set")
                    }
                };
                sock.send_to(line.as_bytes(), resolved)
                    .with_context(|| format!("send UDP syslog datagram to '{addr}'"))?;
                Ok(())
            }
            SyslogTarget::Tcp(addr) => {
                let framed = format!("{line}\n");
                if let Some(stream) = &mut self.tcp_stream {
                    if tcp_stream_is_alive(stream) && stream.write_all(framed.as_bytes()).is_ok() {
                        return Ok(());
                    }
                    self.tcp_stream = None;
                }
                let resolved = addr
                    .to_socket_addrs()
                    .with_context(|| format!("resolve TCP syslog target '{addr}'"))?
                    .next()
                    .ok_or_else(|| anyhow!("no addresses resolved for TCP syslog '{addr}'"))?;
                let mut stream = TcpStream::connect_timeout(&resolved, SYSLOG_IO_TIMEOUT)
                    .with_context(|| format!("connect TCP syslog '{addr}'"))?;
                let _ = stream.set_write_timeout(Some(SYSLOG_IO_TIMEOUT));
                stream
                    .write_all(framed.as_bytes())
                    .with_context(|| format!("write TCP syslog to '{addr}'"))?;
                self.tcp_stream = Some(stream);
                Ok(())
            }
            #[cfg(unix)]
            SyslogTarget::Unix(path) => {
                if let Some(sock) = &self.unix_datagram {
                    if sock.send_to(line.as_bytes(), path).is_ok() {
                        return Ok(());
                    }
                    self.unix_datagram = None;
                }
                if let Some(stream) = &mut self.unix_stream {
                    let framed = format!("{line}\n");
                    if stream.write_all(framed.as_bytes()).is_ok() {
                        return Ok(());
                    }
                    self.unix_stream = None;
                }
                if let Ok(sock) = UnixDatagram::unbound() {
                    let _ = sock.set_write_timeout(Some(SYSLOG_IO_TIMEOUT));
                    if sock.send_to(line.as_bytes(), path).is_ok() {
                        self.unix_datagram = Some(sock);
                        return Ok(());
                    }
                }
                let mut stream = UnixStream::connect(path)
                    .with_context(|| format!("connect Unix syslog '{}'", path.display()))?;
                let _ = stream.set_write_timeout(Some(SYSLOG_IO_TIMEOUT));
                stream
                    .write_all(format!("{line}\n").as_bytes())
                    .with_context(|| format!("write Unix syslog '{}'", path.display()))?;
                self.unix_stream = Some(stream);
                Ok(())
            }
        }
    }
}

fn tcp_stream_is_alive(stream: &mut TcpStream) -> bool {
    if stream.set_nonblocking(true).is_err() {
        return false;
    }
    let mut probe = [0u8; 1];
    let alive = match stream.read(&mut probe) {
        Ok(0) => false,
        Ok(_) => true,
        Err(err) if err.kind() == ErrorKind::WouldBlock => true,
        Err(_) => false,
    };
    if stream.set_nonblocking(false).is_err() {
        return false;
    }
    alive
}

pub fn format_syslog_message(
    event: &LogEvent,
    format: SyslogFormat,
    facility_override: Option<SyslogFacility>,
    hostname: &str,
    pid: u32,
) -> String {
    let facility = facility_override.unwrap_or(match event.category {
        LogCategory::ServerError => SyslogFacility::Daemon,
        LogCategory::SecurityAudit | LogCategory::TaskState => SyslogFacility::Auth,
    });
    let pri = (facility.code() as u16) * 8 + (event.severity.syslog_code() as u16);
    let safe_host = sanitize_syslog_token(hostname, 255);
    let msg = truncate_chars(&sanitize_inline(&event.message), MAX_EVENT_MESSAGE_CHARS);

    match format {
        SyslogFormat::Rfc5424 => {
            let timestamp = format_rfc3339_utc(event.timestamp_secs);
            let msgid = sanitize_syslog_token(&event.action, 32);
            let mut sd_params = vec![
                format!("category=\"{}\"", escape_sd_param(event.category.as_str())),
                format!("severity=\"{}\"", escape_sd_param(event.severity.as_str())),
                format!("action=\"{}\"", escape_sd_param(&event.action)),
            ];
            if let Some(outcome) = &event.outcome {
                sd_params.push(format!("outcome=\"{}\"", escape_sd_param(outcome)));
            }
            if let Some(actor) = &event.actor {
                sd_params.push(format!("actor=\"{}\"", escape_sd_param(actor)));
            }
            if let Some(target) = &event.target {
                sd_params.push(format!("target=\"{}\"", escape_sd_param(target)));
            }
            if let Some(channel) = &event.channel {
                sd_params.push(format!("channel=\"{}\"", escape_sd_param(channel)));
            }
            format!(
                "<{pri}>1 {timestamp} {safe_host} aircd {pid} {msgid} [aircd@32473 {sd}] {msg}",
                sd = sd_params.join(" ")
            )
        }
        SyslogFormat::Rfc3164 => {
            let timestamp = format_rfc3164_utc(event.timestamp_secs);
            let action = sanitize_inline(&event.action);
            let mut details = vec![format!("category={}", event.category.as_str()), format!("action={action}")];
            if let Some(outcome) = &event.outcome {
                details.push(format!("outcome={}", sanitize_inline(outcome)));
            }
            if let Some(actor) = &event.actor {
                details.push(format!("actor={}", sanitize_inline(actor)));
            }
            if let Some(target) = &event.target {
                details.push(format!("target={}", sanitize_inline(target)));
            }
            if let Some(channel) = &event.channel {
                details.push(format!("channel={}", sanitize_inline(channel)));
            }
            format!(
                "<{pri}>{timestamp} {safe_host} aircd[{pid}]: [{}] {msg}",
                details.join(" ")
            )
        }
    }
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn detect_hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .map(|s| sanitize_syslog_token(&s, 255))
        .filter(|s| s != "-")
        .unwrap_or_else(|| "localhost".to_string())
}

fn sanitize_inline(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c == '\r' || c == '\n' || c == '\0' || c.is_control() {
                ' '
            } else {
                c
            }
        })
        .collect::<String>()
        .trim()
        .to_string()
}

fn sanitize_syslog_token(raw: &str, max_len: usize) -> String {
    let cleaned: String = raw
        .trim()
        .chars()
        .filter(|c| c.is_ascii_graphic())
        .take(max_len)
        .collect();
    if cleaned.is_empty() {
        "-".to_string()
    } else {
        cleaned
    }
}

fn escape_sd_param(raw: &str) -> String {
    let inline = sanitize_inline(raw);
    let mut out = String::with_capacity(inline.len());
    for ch in inline.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            ']' => out.push_str("\\]"),
            _ => out.push(ch),
        }
    }
    out
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let kept: String = text.chars().take(max_chars.saturating_sub(3)).collect();
    format!("{kept}...")
}

fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    // Howard Hinnant's civil-from-days algorithm.
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = y + if m <= 2 { 1 } else { 0 };
    (year, m as u32, d as u32)
}

fn utc_components(unix_secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (unix_secs / 86_400) as i64;
    let rem = unix_secs % 86_400;
    let hour = (rem / 3600) as u32;
    let minute = ((rem % 3600) / 60) as u32;
    let second = (rem % 60) as u32;
    let (year, month, day) = civil_from_days(days);
    (year, month, day, hour, minute, second)
}

fn format_rfc3339_utc(unix_secs: u64) -> String {
    let (year, month, day, hour, minute, second) = utc_components(unix_secs);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn format_rfc3164_utc(unix_secs: u64) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let (_year, month, day, hour, minute, second) = utc_components(unix_secs);
    let mon = MONTHS[(month.saturating_sub(1) as usize).min(11)];
    format!("{mon} {day:2} {hour:02}:{minute:02}:{second:02}")
}

#[cfg(windows)]
#[allow(clippy::upper_case_acronyms)]
mod win_event_log {
    use super::Severity;
    use anyhow::{anyhow, Result};
    use std::ffi::c_void;
    use std::ptr;

    type HANDLE = *mut c_void;
    type BOOL = i32;
    type WORD = u16;
    type DWORD = u32;
    type LPCWSTR = *const u16;

    const EVENTLOG_ERROR_TYPE: WORD = 0x0001;
    const EVENTLOG_WARNING_TYPE: WORD = 0x0002;
    const EVENTLOG_INFORMATION_TYPE: WORD = 0x0004;

    #[link(name = "advapi32")]
    extern "system" {
        fn RegisterEventSourceW(lpUNCServerName: LPCWSTR, lpSourceName: LPCWSTR) -> HANDLE;
        fn DeregisterEventSource(hEventLog: HANDLE) -> BOOL;
        fn ReportEventW(
            hEventLog: HANDLE,
            wType: WORD,
            wCategory: WORD,
            dwEventID: DWORD,
            lpUserSid: *mut c_void,
            wNumStrings: WORD,
            dwDataSize: DWORD,
            lpStrings: *const LPCWSTR,
            lpRawData: *const c_void,
        ) -> BOOL;
    }

    pub struct EventSource {
        handle: HANDLE,
    }

    // Win32 event log handles are thread-safe to use from the dedicated forwarder thread.
    unsafe impl Send for EventSource {}

    impl EventSource {
        pub fn register(source_name: &str) -> Result<Self> {
            let wide: Vec<u16> = source_name
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let handle = unsafe { RegisterEventSourceW(ptr::null(), wide.as_ptr()) };
            if handle.is_null() {
                return Err(anyhow!(
                    "RegisterEventSourceW('{source_name}') failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
            Ok(Self { handle })
        }

        pub fn report(
            &self,
            severity: Severity,
            category: u16,
            event_id: u32,
            message: &str,
        ) -> Result<()> {
            let event_type = match severity {
                Severity::Error => EVENTLOG_ERROR_TYPE,
                Severity::Warning => EVENTLOG_WARNING_TYPE,
                Severity::Info => EVENTLOG_INFORMATION_TYPE,
            };
            let wide: Vec<u16> = message
                .chars()
                .filter(|&c| c != '\0')
                .collect::<String>()
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let strings = [wide.as_ptr()];
            let ok = unsafe {
                ReportEventW(
                    self.handle,
                    event_type,
                    category,
                    event_id,
                    ptr::null_mut(),
                    1,
                    0,
                    strings.as_ptr(),
                    ptr::null(),
                )
            };
            if ok == 0 {
                return Err(anyhow!(
                    "ReportEventW failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
            Ok(())
        }
    }

    impl Drop for EventSource {
        fn drop(&mut self) {
            if !self.handle.is_null() {
                unsafe {
                    DeregisterEventSource(self.handle);
                }
                self.handle = ptr::null_mut();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;

    #[test]
    fn syslog_target_parser_handles_udp_tcp_and_rejects_invalid() -> Result<()> {
        assert_eq!(
            SyslogTarget::parse("127.0.0.1:514")?,
            SyslogTarget::Udp("127.0.0.1:514".to_string())
        );
        assert_eq!(
            SyslogTarget::parse("udp://[::1]:5514")?,
            SyslogTarget::Udp("[::1]:5514".to_string())
        );
        assert_eq!(
            SyslogTarget::parse("tcp://logs.example.com:6514")?,
            SyslogTarget::Tcp("logs.example.com:6514".to_string())
        );
        for bad in [
            "",
            " ",
            "127.0.0.1",
            "udp://127.0.0.1:0",
            "tcp://:514",
            "http://127.0.0.1:514",
            "udp://bad host:514",
        ] {
            assert!(SyslogTarget::parse(bad).is_err(), "expected error for {bad:?}");
        }
        Ok(())
    }

    #[test]
    fn syslog_formats_rfc5424_and_rfc3164_with_escaped_structured_data() {
        let mut event = LogEvent::security_audit(
            Severity::Warning,
            "auth.http_failed",
            "failure",
            "Invalid bearer \"token\"\nline2]",
        )
        .with_actor("agent-a")
        .with_target("peer\\1")
        .with_channel("/v1/me");
        event.timestamp_secs = 1_794_605_400; // 2026-11-13T21:30:00Z

        let rfc5424 = format_syslog_message(&event, SyslogFormat::Rfc5424, None, "hub-1", 42);
        // auth(4) * 8 + warning(4) = 36
        assert!(rfc5424.starts_with("<36>1 2026-11-13T21:30:00Z hub-1 aircd 42 auth.http_failed "));
        assert!(rfc5424.contains("[aircd@32473 category=\"security_audit\" severity=\"warning\" action=\"auth.http_failed\" outcome=\"failure\" actor=\"agent-a\" target=\"peer\\\\1\" channel=\"/v1/me\"]"));
        assert!(!rfc5424.contains('\n'));

        let rfc3164 = format_syslog_message(
            &event,
            SyslogFormat::Rfc3164,
            Some(SyslogFacility::Local0),
            "hub-1",
            42,
        );
        // local0(16) * 8 + warning(4) = 132
        assert!(rfc3164.starts_with("<132>Nov 13 21:30:00 hub-1 aircd[42]: "));
        assert!(rfc3164.contains("action=auth.http_failed"));
    }

    #[test]
    fn forwarder_sends_server_errors_and_security_audits_over_udp_syslog() -> Result<()> {
        let receiver = UdpSocket::bind("127.0.0.1:0")?;
        receiver.set_read_timeout(Some(Duration::from_secs(3)))?;
        let addr = receiver.local_addr()?;

        let forwarder = EventForwarder::new(LogConfig {
            syslog_target: Some(SyslogTarget::Udp(addr.to_string())),
            syslog_format: SyslogFormat::Rfc5424,
            stderr_errors: false,
            ..LogConfig::default()
        })?;

        forwarder.log_error("irc.accept_error", "connection reset by peer");
        forwarder.log_security(
            Severity::Warning,
            "auth.http_failed",
            "failure",
            Some("unknown"),
            None,
            Some("/v1/messages"),
            "Invalid bearer token",
        );
        forwarder.flush();

        let mut buf = [0u8; 4096];
        let (n1, _) = receiver.recv_from(&mut buf)?;
        let msg1 = String::from_utf8_lossy(&buf[..n1]).into_owned();
        let (n2, _) = receiver.recv_from(&mut buf)?;
        let msg2 = String::from_utf8_lossy(&buf[..n2]).into_owned();

        // daemon(3)*8 + error(3) = 27
        assert!(msg1.starts_with("<27>1 "), "got {msg1}");
        assert!(msg1.contains("irc.accept_error"));
        assert!(msg1.contains("connection reset by peer"));

        // auth(4)*8 + warning(4) = 36
        assert!(msg2.starts_with("<36>1 "), "got {msg2}");
        assert!(msg2.contains("auth.http_failed"));
        assert!(msg2.contains("channel=\"/v1/messages\""));
        Ok(())
    }

    #[test]
    fn forwarder_sends_events_over_tcp_syslog_and_recovers_if_connection_drops() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;

        let (line_tx, line_rx) = mpsc::channel();
        let server_thread = std::thread::spawn(move || {
            let (stream1, _) = listener.accept().expect("first accept");
            let mut reader1 = BufReader::new(stream1);
            let mut first = String::new();
            reader1.read_line(&mut first).expect("read first line");
            line_tx.send(first).expect("send first line");
            drop(reader1);

            let (stream2, _) = listener.accept().expect("second accept");
            let mut reader2 = BufReader::new(stream2);
            let mut second = String::new();
            reader2.read_line(&mut second).expect("read second line");
            line_tx.send(second).expect("send second line");
        });

        let forwarder = EventForwarder::new(LogConfig {
            syslog_target: Some(SyslogTarget::Tcp(addr.to_string())),
            syslog_format: SyslogFormat::Rfc3164,
            syslog_facility: Some(SyslogFacility::Local4),
            stderr_errors: false,
            ..LogConfig::default()
        })?;

        forwarder.log_security(
            Severity::Info,
            "token.reissued",
            "success",
            Some("owner"),
            Some("worker-1"),
            Some("/v1/owner/reissue"),
            "Owner reissued token",
        );
        forwarder.flush();
        let first = line_rx.recv_timeout(Duration::from_secs(3))?;
        // local4(20)*8 + info(6) = 166
        assert!(first.starts_with("<166>"), "got {first}");
        assert!(first.contains("token.reissued"));

        // Wait briefly for server_thread to drop stream1, then send another event to trigger reconnect.
        std::thread::sleep(Duration::from_millis(50));
        forwarder.log_error("webhook.worker_error", "temporary failure");
        forwarder.flush();
        let second = line_rx.recv_timeout(Duration::from_secs(3))?;
        // local4(20)*8 + error(3) = 163
        assert!(second.starts_with("<163>"), "got {second}");
        assert!(second.contains("webhook.worker_error"));

        server_thread.join().expect("tcp syslog test thread");
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn windows_event_log_reports_error_and_security_events_without_failing() -> Result<()> {
        let source = win_event_log::EventSource::register("aircd")?;
        source.report(
            Severity::Info,
            LogCategory::SecurityAudit.windows_category_id(),
            2000,
            "aircd unit test security event",
        )?;
        source.report(
            Severity::Error,
            LogCategory::ServerError.windows_category_id(),
            1000,
            "aircd unit test error event",
        )?;
        Ok(())
    }

    #[test]
    fn audit_stream_classifies_filters_and_replays_without_duplicates() -> Result<()> {
        let forwarder = EventForwarder::default();
        forwarder.log_security(
            Severity::Warning,
            "auth.http_failed",
            "failure",
            Some("alice"),
            None,
            Some("/v1/me"),
            "invalid token",
        );
        forwarder.log_security(
            Severity::Info,
            "role.changed",
            "success",
            Some("owner"),
            Some("bob"),
            Some("#cams"),
            "role updated",
        );
        forwarder.log_task_state(
            Severity::Info,
            "task.status",
            "task_123",
            Some("alice"),
            Some("#cams"),
            Some("in_progress"),
            Some("testing"),
            Some(serde_json::json!({"branch": "feat/x"})),
            "moved to testing",
        );
        forwarder.log_error("http.internal_error", "db unavailable");

        assert_eq!(forwarder.audit_current_seq(), 4);

        let (replayed, mut rx, seq_now) = forwarder.audit_subscribe(Some(1), None);
        assert_eq!(seq_now, 4);
        assert_eq!(replayed.len(), 3);
        assert_eq!(replayed[0].category, "security");
        assert_eq!(replayed[1].category, "task_state");
        assert_eq!(replayed[1].task_id.as_deref(), Some("task_123"));
        assert_eq!(replayed[1].from_status.as_deref(), Some("in_progress"));
        assert_eq!(replayed[1].to_status.as_deref(), Some("testing"));
        assert_eq!(replayed[2].category, "server_error");

        // Live event after subscribe arrives with seq 5.
        forwarder.log_security(
            Severity::Info,
            "token.issued",
            "success",
            Some("owner"),
            Some("alice"),
            Some("/v1/tokens"),
            "issued token",
        );
        let live = rx.try_recv()?;
        assert_eq!(live.seq, 5);
        assert_eq!(live.category, "auth");

        // Filter tests: categories, action prefix, actor, channel, task_id.
        let (all, _, _) = forwarder.audit_subscribe(None, Some(10));
        assert_eq!(all.len(), 5);

        let mut f_auth = AuditStreamFilter::default();
        f_auth.parse_categories("auth")?;
        let auth_only: Vec<_> = all.iter().filter(|e| f_auth.matches(e)).collect();
        assert_eq!(auth_only.len(), 2);

        let mut f_sec_audit = AuditStreamFilter::default();
        f_sec_audit.parse_categories("security_audit")?;
        assert_eq!(all.iter().filter(|e| f_sec_audit.matches(e)).count(), 3);

        let mut f_task = AuditStreamFilter::default();
        f_task.parse_categories("task")?;
        f_task.set_channel("cams");
        f_task.set_actor("@ALICE");
        f_task.set_task_id("task_123");
        f_task.parse_actions("task.*");
        assert_eq!(all.iter().filter(|e| f_task.matches(e)).count(), 1);

        assert!(f_task.parse_categories("bogus").is_err());
        assert!(f_task.parse_severities("bogus").is_err());
        Ok(())
    }
}
