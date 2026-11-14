//! Configurable Prometheus alerting rules exporter, evaluator, and webhook notifier.

use anyhow::{anyhow, bail, Context, Result};
use reqwest::redirect::Policy;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::VecDeque;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use uuid::Uuid;

use crate::history::unix_timestamp;
use crate::logging::{EventForwarder, Severity};
use crate::metrics::{AlertMetricSample, DiskStats, MetricsRegistry, WakeFailureWindowStats};
use crate::webhooks::{sign_payload, validate_webhook_url, WebhookStore};

pub const DEFAULT_ALERTS_ENABLED: bool = true;
pub const DEFAULT_ALERTS_EVAL_INTERVAL_SECS: u64 = 15;
pub const DEFAULT_ALERTS_REPEAT_INTERVAL_SECS: u64 = 3600;
pub const DEFAULT_ALERTS_SEND_RESOLVED: bool = true;
pub const DEFAULT_ALERTS_GROUP_NAME: &str = "aircd_alerts";

pub const DEFAULT_WAKE_FAILURE_ENABLED: bool = true;
pub const DEFAULT_WAKE_FAILURE_RATE_THRESHOLD: f64 = 0.25;
pub const DEFAULT_WAKE_FAILURE_WINDOW_SECS: u64 = 300;
pub const DEFAULT_WAKE_FAILURE_MIN_SAMPLES: u64 = 5;
pub const DEFAULT_WAKE_FAILURE_FOR_SECS: u64 = 120;
pub const DEFAULT_WAKE_FAILURE_SEVERITY: &str = "critical";

pub const DEFAULT_QUEUE_BACKLOG_ENABLED: bool = true;
pub const DEFAULT_QUEUE_BACKLOG_THRESHOLD: u64 = 50;
pub const DEFAULT_QUEUE_BACKLOG_FOR_SECS: u64 = 60;
pub const DEFAULT_QUEUE_BACKLOG_SEVERITY: &str = "warning";

pub const DEFAULT_DISK_PRESSURE_ENABLED: bool = true;
pub const DEFAULT_DISK_USED_RATIO_THRESHOLD: f64 = 0.90;
pub const DEFAULT_DISK_MIN_FREE_BYTES: u64 = 1_073_741_824; // 1 GiB
pub const DEFAULT_DISK_PRESSURE_FOR_SECS: u64 = 60;
pub const DEFAULT_DISK_PRESSURE_SEVERITY: &str = "critical";

const MAX_ALERT_DELIVERY_HISTORY: usize = 100;
const ALLOWED_SEVERITIES: &[&str] = &["info", "warning", "critical"];

#[derive(Clone, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct AlertConfig {
    /// Master switch for the background alert evaluation worker.
    pub enabled: bool,
    /// Interval in seconds between background alert evaluations (`0` disables timer).
    pub eval_interval_secs: u64,
    /// Optional direct webhook URL for alert notifications.
    pub webhook_url: Option<String>,
    /// Optional HMAC-SHA256 signing secret for direct alert webhook deliveries.
    #[serde(skip_serializing)]
    pub webhook_secret: Option<String>,
    /// Seconds between repeat webhook notifications while an alert remains firing (`0` disables repeats).
    pub repeat_interval_secs: u64,
    /// Emit a webhook notification when a firing alert transitions back to resolved.
    pub send_resolved: bool,
    /// Prometheus rule group name used in exported YAML rules.
    pub group_name: String,

    // Sustained high wake failure rate rule
    pub wake_failure_enabled: bool,
    pub wake_failure_rate_threshold: f64,
    pub wake_failure_window_secs: u64,
    pub wake_failure_min_samples: u64,
    pub wake_failure_for_secs: u64,
    pub wake_failure_severity: String,

    // Queue backlog rule (pending wakes + pending webhook deliveries)
    pub queue_backlog_enabled: bool,
    pub queue_backlog_threshold: u64,
    pub queue_backlog_for_secs: u64,
    pub queue_backlog_severity: String,

    // Disk pressure rule (DB / configured volume capacity)
    pub disk_pressure_enabled: bool,
    pub disk_used_ratio_threshold: f64,
    pub disk_min_free_bytes: u64,
    pub disk_pressure_for_secs: u64,
    pub disk_pressure_severity: String,
    pub disk_path: Option<String>,
}

impl fmt::Debug for AlertConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AlertConfig")
            .field("enabled", &self.enabled)
            .field("eval_interval_secs", &self.eval_interval_secs)
            .field("webhook_url", &self.webhook_url)
            .field(
                "webhook_secret",
                &self.webhook_secret.as_ref().map(|_| "<redacted>"),
            )
            .field("repeat_interval_secs", &self.repeat_interval_secs)
            .field("send_resolved", &self.send_resolved)
            .field("group_name", &self.group_name)
            .field("wake_failure_enabled", &self.wake_failure_enabled)
            .field(
                "wake_failure_rate_threshold",
                &self.wake_failure_rate_threshold,
            )
            .field("wake_failure_window_secs", &self.wake_failure_window_secs)
            .field("wake_failure_min_samples", &self.wake_failure_min_samples)
            .field("wake_failure_for_secs", &self.wake_failure_for_secs)
            .field("wake_failure_severity", &self.wake_failure_severity)
            .field("queue_backlog_enabled", &self.queue_backlog_enabled)
            .field("queue_backlog_threshold", &self.queue_backlog_threshold)
            .field("queue_backlog_for_secs", &self.queue_backlog_for_secs)
            .field("queue_backlog_severity", &self.queue_backlog_severity)
            .field("disk_pressure_enabled", &self.disk_pressure_enabled)
            .field(
                "disk_used_ratio_threshold",
                &self.disk_used_ratio_threshold,
            )
            .field("disk_min_free_bytes", &self.disk_min_free_bytes)
            .field("disk_pressure_for_secs", &self.disk_pressure_for_secs)
            .field("disk_pressure_severity", &self.disk_pressure_severity)
            .field("disk_path", &self.disk_path)
            .finish()
    }
}

impl Default for AlertConfig {
    fn default() -> Self {
        Self {
            enabled: DEFAULT_ALERTS_ENABLED,
            eval_interval_secs: DEFAULT_ALERTS_EVAL_INTERVAL_SECS,
            webhook_url: None,
            webhook_secret: None,
            repeat_interval_secs: DEFAULT_ALERTS_REPEAT_INTERVAL_SECS,
            send_resolved: DEFAULT_ALERTS_SEND_RESOLVED,
            group_name: DEFAULT_ALERTS_GROUP_NAME.to_string(),
            wake_failure_enabled: DEFAULT_WAKE_FAILURE_ENABLED,
            wake_failure_rate_threshold: DEFAULT_WAKE_FAILURE_RATE_THRESHOLD,
            wake_failure_window_secs: DEFAULT_WAKE_FAILURE_WINDOW_SECS,
            wake_failure_min_samples: DEFAULT_WAKE_FAILURE_MIN_SAMPLES,
            wake_failure_for_secs: DEFAULT_WAKE_FAILURE_FOR_SECS,
            wake_failure_severity: DEFAULT_WAKE_FAILURE_SEVERITY.to_string(),
            queue_backlog_enabled: DEFAULT_QUEUE_BACKLOG_ENABLED,
            queue_backlog_threshold: DEFAULT_QUEUE_BACKLOG_THRESHOLD,
            queue_backlog_for_secs: DEFAULT_QUEUE_BACKLOG_FOR_SECS,
            queue_backlog_severity: DEFAULT_QUEUE_BACKLOG_SEVERITY.to_string(),
            disk_pressure_enabled: DEFAULT_DISK_PRESSURE_ENABLED,
            disk_used_ratio_threshold: DEFAULT_DISK_USED_RATIO_THRESHOLD,
            disk_min_free_bytes: DEFAULT_DISK_MIN_FREE_BYTES,
            disk_pressure_for_secs: DEFAULT_DISK_PRESSURE_FOR_SECS,
            disk_pressure_severity: DEFAULT_DISK_PRESSURE_SEVERITY.to_string(),
            disk_path: None,
        }
    }
}

impl AlertConfig {
    pub fn validate(&mut self) -> Result<()> {
        self.group_name = self.group_name.trim().to_string();
        if self.group_name.is_empty()
            || self
                .group_name
                .chars()
                .any(|c| c.is_whitespace() || c.is_control())
        {
            bail!("alerts.group_name must be a non-empty identifier without whitespace");
        }
        if let Some(url) = &mut self.webhook_url {
            if url.trim().is_empty() {
                bail!("alerts.webhook_url must not be empty");
            }
            *url = validate_webhook_url(url).context("invalid alerts.webhook_url")?;
        }
        if let Some(secret) = &self.webhook_secret {
            if secret.trim().is_empty() {
                bail!("alerts.webhook_secret must not be empty");
            }
        }
        if let Some(path) = &self.disk_path {
            if path.trim().is_empty() {
                bail!("alerts.disk_path must not be empty");
            }
        }
        if !self.wake_failure_rate_threshold.is_finite()
            || !(0.0..=1.0).contains(&self.wake_failure_rate_threshold)
        {
            bail!(
                "alerts.wake_failure_rate_threshold must be between 0.0 and 1.0 (got {})",
                self.wake_failure_rate_threshold
            );
        }
        if self.wake_failure_window_secs == 0 {
            bail!("alerts.wake_failure_window_secs must be at least 1");
        }
        if !self.disk_used_ratio_threshold.is_finite()
            || !(0.0..=1.0).contains(&self.disk_used_ratio_threshold)
        {
            bail!(
                "alerts.disk_used_ratio_threshold must be between 0.0 and 1.0 (got {})",
                self.disk_used_ratio_threshold
            );
        }
        self.wake_failure_severity =
            normalize_severity("alerts.wake_failure_severity", &self.wake_failure_severity)?;
        self.queue_backlog_severity =
            normalize_severity("alerts.queue_backlog_severity", &self.queue_backlog_severity)?;
        self.disk_pressure_severity =
            normalize_severity("alerts.disk_pressure_severity", &self.disk_pressure_severity)?;
        Ok(())
    }
}

fn normalize_severity(field: &str, raw: &str) -> Result<String> {
    let clean = raw.trim().to_ascii_lowercase();
    if ALLOWED_SEVERITIES.contains(&clean.as_str()) {
        Ok(clean)
    } else {
        bail!(
            "{field} must be one of: {} (got '{raw}')",
            ALLOWED_SEVERITIES.join(", ")
        );
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AlertState {
    Inactive,
    Pending,
    Firing,
}

impl AlertState {
    pub fn code(self) -> u8 {
        match self {
            Self::Inactive => 0,
            Self::Pending => 1,
            Self::Firing => 2,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct AlertEvalSnapshot {
    pub wake_failure: WakeFailureWindowStats,
    pub wake_pending: u64,
    pub webhook_pending: u64,
    pub queue_backlog_total: u64,
    pub disk: Option<DiskStats>,
}

#[derive(Clone, Debug, Serialize)]
pub struct AlertRuleStatus {
    pub rule: String,
    pub alertname: String,
    pub enabled: bool,
    pub severity: String,
    pub state: AlertState,
    pub for_secs: u64,
    pub current_value: f64,
    pub threshold: f64,
    pub condition_met: bool,
    pub expr: String,
    pub summary: String,
    pub description: String,
    pub pending_since: Option<i64>,
    pub active_at: Option<i64>,
    pub resolved_at: Option<i64>,
    pub last_notified_at: Option<i64>,
    pub details: serde_json::Value,
}

#[derive(Clone, Debug, Serialize)]
pub struct AlertDeliveryRecord {
    pub delivery_id: String,
    pub target: String,
    pub event_type: String,
    pub rule: String,
    pub alertname: String,
    pub alert_status: String,
    pub status: String,
    pub http_status: Option<i64>,
    pub error: Option<String>,
    pub created_at: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct AlertStatusSnapshot {
    pub enabled: bool,
    pub eval_interval_secs: u64,
    pub webhook_configured: bool,
    pub webhook_url: Option<String>,
    pub webhook_secret_configured: bool,
    pub last_evaluated_at: Option<i64>,
    pub evaluation_count: u64,
    pub active_count: usize,
    pub pending_count: usize,
    pub rules: Vec<AlertRuleStatus>,
    pub metrics: AlertEvalSnapshot,
    pub recent_deliveries: Vec<AlertDeliveryRecord>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PrometheusAlertRule {
    pub alert: String,
    pub expr: String,
    #[serde(rename = "for")]
    pub for_duration: String,
    pub labels: std::collections::BTreeMap<String, String>,
    pub annotations: std::collections::BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PrometheusRuleGroup {
    pub name: String,
    pub interval: String,
    pub rules: Vec<PrometheusAlertRule>,
}

#[derive(Clone, Debug)]
struct RuleEvaluationInput {
    rule: &'static str,
    alertname: &'static str,
    enabled: bool,
    severity: String,
    for_secs: u64,
    condition_met: bool,
    current_value: f64,
    threshold: f64,
    expr: String,
    summary: String,
    description: String,
    details: serde_json::Value,
}

#[derive(Clone, Debug)]
struct RuleRuntimeState {
    rule: &'static str,
    alertname: &'static str,
    enabled: bool,
    severity: String,
    for_secs: u64,
    state: AlertState,
    pending_since_mono: Option<Instant>,
    pending_since: Option<i64>,
    active_at: Option<i64>,
    resolved_at: Option<i64>,
    last_notified_mono: Option<Instant>,
    last_notified_at: Option<i64>,
    last_value: f64,
    threshold: f64,
    last_condition_met: bool,
    expr: String,
    summary: String,
    description: String,
    details: serde_json::Value,
}

impl RuleRuntimeState {
    fn new(rule: &'static str, alertname: &'static str, severity: &str, for_secs: u64) -> Self {
        Self {
            rule,
            alertname,
            enabled: true,
            severity: severity.to_string(),
            for_secs,
            state: AlertState::Inactive,
            pending_since_mono: None,
            pending_since: None,
            active_at: None,
            resolved_at: None,
            last_notified_mono: None,
            last_notified_at: None,
            last_value: 0.0,
            threshold: 0.0,
            last_condition_met: false,
            expr: String::new(),
            summary: String::new(),
            description: String::new(),
            details: json!({}),
        }
    }

    fn to_status(&self) -> AlertRuleStatus {
        AlertRuleStatus {
            rule: self.rule.to_string(),
            alertname: self.alertname.to_string(),
            enabled: self.enabled,
            severity: self.severity.clone(),
            state: self.state,
            for_secs: self.for_secs,
            current_value: self.last_value,
            threshold: self.threshold,
            condition_met: self.last_condition_met,
            expr: self.expr.clone(),
            summary: self.summary.clone(),
            description: self.description.clone(),
            pending_since: self.pending_since,
            active_at: self.active_at,
            resolved_at: self.resolved_at,
            last_notified_at: self.last_notified_at,
            details: self.details.clone(),
        }
    }

    fn to_metric_sample(&self) -> AlertMetricSample {
        AlertMetricSample {
            rule: self.rule.to_string(),
            alertname: self.alertname.to_string(),
            severity: self.severity.clone(),
            state_code: self.state.code(),
            active: self.state == AlertState::Firing,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PendingAlertNotification {
    pub rule: String,
    pub alertname: String,
    pub alert_status: String,
    pub severity: String,
    pub summary: String,
    pub description: String,
    pub value: f64,
    pub threshold: f64,
    pub starts_at: i64,
    pub ends_at: Option<i64>,
    pub details: serde_json::Value,
}

struct AlertInner {
    config: AlertConfig,
    db_path: Option<PathBuf>,
    rules: [RuleRuntimeState; 3],
    last_evaluated_at: Option<i64>,
    evaluation_count: u64,
    deliveries: VecDeque<AlertDeliveryRecord>,
}

#[derive(Clone)]
pub struct AlertManager {
    inner: Arc<RwLock<AlertInner>>,
    logger: EventForwarder,
    metrics: Arc<MetricsRegistry>,
    webhooks: WebhookStore,
    http_client: reqwest::Client,
    notify: Arc<Notify>,
}

impl AlertManager {
    pub fn new(
        config: AlertConfig,
        db_path: Option<PathBuf>,
        logger: EventForwarder,
        metrics: Arc<MetricsRegistry>,
        webhooks: WebhookStore,
    ) -> Self {
        let http_client = reqwest::Client::builder()
            .redirect(Policy::none())
            .timeout(Duration::from_secs(10))
            .build()
            .expect("build reqwest alert client");

        let rules = [
            RuleRuntimeState::new(
                "wake_failure_rate",
                "AircdHighWakeFailureRate",
                &config.wake_failure_severity,
                config.wake_failure_for_secs,
            ),
            RuleRuntimeState::new(
                "queue_backlog",
                "AircdQueueBacklogHigh",
                &config.queue_backlog_severity,
                config.queue_backlog_for_secs,
            ),
            RuleRuntimeState::new(
                "disk_pressure",
                "AircdDiskPressureHigh",
                &config.disk_pressure_severity,
                config.disk_pressure_for_secs,
            ),
        ];

        Self {
            inner: Arc::new(RwLock::new(AlertInner {
                config,
                db_path,
                rules,
                last_evaluated_at: None,
                evaluation_count: 0,
                deliveries: VecDeque::new(),
            })),
            logger,
            metrics,
            webhooks,
            http_client,
            notify: Arc::new(Notify::new()),
        }
    }

    pub fn config(&self) -> AlertConfig {
        self.inner
            .read()
            .expect("alert config lock poisoned")
            .config
            .clone()
    }

    pub fn update_config(&self, config: AlertConfig) {
        {
            let mut guard = self.inner.write().expect("alert config lock poisoned");
            guard.config = config;
        }
        self.notify.notify_waiters();
    }

    pub fn notify_handle(&self) -> &Arc<Notify> {
        &self.notify
    }

    pub fn effective_disk_path(&self) -> Option<PathBuf> {
        let guard = self.inner.read().expect("alert config lock poisoned");
        if let Some(explicit) = &guard.config.disk_path {
            return Some(PathBuf::from(explicit));
        }
        guard.db_path.clone()
    }

    pub fn sample_disk(&self) -> Option<DiskStats> {
        let path = self.effective_disk_path()?;
        sample_disk_stats(&path)
    }

    pub fn metric_samples(&self) -> Vec<AlertMetricSample> {
        let guard = self.inner.read().expect("alert config lock poisoned");
        guard
            .rules
            .iter()
            .map(RuleRuntimeState::to_metric_sample)
            .collect()
    }

    pub fn status(&self, live_snap: &AlertEvalSnapshot) -> AlertStatusSnapshot {
        let guard = self.inner.read().expect("alert config lock poisoned");
        let cfg = &guard.config;
        let eval_inputs = build_rule_inputs(cfg, live_snap);
        let mut rules = Vec::with_capacity(guard.rules.len());
        let mut active_count = 0usize;
        let mut pending_count = 0usize;
        for (idx, state) in guard.rules.iter().enumerate() {
            let mut item = state.to_status();
            let input = &eval_inputs[idx];
            item.enabled = input.enabled;
            item.severity = input.severity.clone();
            item.for_secs = input.for_secs;
            item.current_value = input.current_value;
            item.threshold = input.threshold;
            item.condition_met = input.condition_met;
            item.expr = input.expr.clone();
            item.summary = input.summary.clone();
            item.description = input.description.clone();
            item.details = input.details.clone();
            if item.state == AlertState::Firing {
                active_count += 1;
            } else if item.state == AlertState::Pending {
                pending_count += 1;
            }
            rules.push(item);
        }
        AlertStatusSnapshot {
            enabled: cfg.enabled,
            eval_interval_secs: cfg.eval_interval_secs,
            webhook_configured: cfg.webhook_url.is_some(),
            webhook_url: cfg.webhook_url.clone(),
            webhook_secret_configured: cfg.webhook_secret.is_some(),
            last_evaluated_at: guard.last_evaluated_at,
            evaluation_count: guard.evaluation_count,
            active_count,
            pending_count,
            rules,
            metrics: live_snap.clone(),
            recent_deliveries: guard.deliveries.iter().cloned().collect(),
        }
    }

    pub fn export_prometheus_rules(&self, include_disabled: bool) -> Vec<PrometheusRuleGroup> {
        let cfg = self.config();
        let dummy = AlertEvalSnapshot::default();
        let inputs = build_rule_inputs(&cfg, &dummy);
        let mut rules = Vec::new();
        for input in inputs {
            if !include_disabled && !input.enabled {
                continue;
            }
            let mut labels = std::collections::BTreeMap::new();
            labels.insert("severity".to_string(), input.severity);
            labels.insert("service".to_string(), "aircd".to_string());
            labels.insert("rule".to_string(), input.rule.to_string());

            let mut annotations = std::collections::BTreeMap::new();
            annotations.insert("summary".to_string(), input.summary);
            annotations.insert("description".to_string(), rule_template_description(&cfg, input.rule));

            rules.push(PrometheusAlertRule {
                alert: input.alertname.to_string(),
                expr: input.expr,
                for_duration: format_prom_duration(input.for_secs),
                labels,
                annotations,
            });
        }

        let interval_secs = cfg.eval_interval_secs.max(1);
        vec![PrometheusRuleGroup {
            name: cfg.group_name,
            interval: format_prom_duration(interval_secs),
            rules,
        }]
    }

    pub fn export_prometheus_rules_yaml(&self, include_disabled: bool) -> String {
        let groups = self.export_prometheus_rules(include_disabled);
        let mut out = String::from("groups:\n");
        for group in groups {
            out.push_str(&format!(
                "  - name: {}\n    interval: {}\n    rules:\n",
                yaml_scalar(&group.name),
                yaml_scalar(&group.interval)
            ));
            if group.rules.is_empty() {
                out.pop();
                out.push_str(" []\n");
                continue;
            }
            for rule in group.rules {
                out.push_str(&format!(
                    "      - alert: {}\n        expr: {}\n        for: {}\n        labels:\n",
                    yaml_scalar(&rule.alert),
                    yaml_scalar(&rule.expr),
                    yaml_scalar(&rule.for_duration),
                ));
                for (k, v) in &rule.labels {
                    out.push_str(&format!("          {k}: {}\n", yaml_scalar(v)));
                }
                out.push_str("        annotations:\n");
                for (k, v) in &rule.annotations {
                    out.push_str(&format!("          {k}: {}\n", yaml_scalar(v)));
                }
            }
        }
        out
    }

    /// Evaluate all alert rules against `snap` at the given monotonic/unix timestamps.
    pub fn evaluate_with_snapshot(
        &self,
        now_mono: Instant,
        now_unix: i64,
        snap: &AlertEvalSnapshot,
        notify: bool,
    ) -> Vec<PendingAlertNotification> {
        let mut pending_notifications = Vec::new();
        {
            let mut guard = self.inner.write().expect("alert state lock poisoned");
            let cfg = guard.config.clone();
            let inputs = build_rule_inputs(&cfg, snap);
            guard.last_evaluated_at = Some(now_unix);
            guard.evaluation_count += 1;

            for (idx, input) in inputs.into_iter().enumerate() {
                let state = &mut guard.rules[idx];
                state.enabled = input.enabled;
                state.severity = input.severity.clone();
                state.for_secs = input.for_secs;
                state.last_value = input.current_value;
                state.threshold = input.threshold;
                state.last_condition_met = input.condition_met;
                state.expr = input.expr;
                state.summary = input.summary.clone();
                state.description = input.description.clone();
                state.details = input.details.clone();

                if !input.enabled {
                    state.state = AlertState::Inactive;
                    state.pending_since_mono = None;
                    state.pending_since = None;
                    state.active_at = None;
                    continue;
                }

                if input.condition_met {
                    match state.state {
                        AlertState::Inactive => {
                            if input.for_secs == 0 {
                                state.state = AlertState::Firing;
                                state.pending_since_mono = Some(now_mono);
                                state.pending_since = Some(now_unix);
                                state.active_at = Some(now_unix);
                                state.last_notified_mono = Some(now_mono);
                                state.last_notified_at = Some(now_unix);
                                self.metrics.record_alert_fired(state.rule);
                                self.logger.log_security(
                                    Severity::Warning,
                                    "alert.firing",
                                    "firing",
                                    Some("system"),
                                    None,
                                    None,
                                    format!(
                                        "{} firing ({}): {}",
                                        state.alertname, state.severity, state.description
                                    ),
                                );
                                if notify {
                                    pending_notifications.push(PendingAlertNotification {
                                        rule: state.rule.to_string(),
                                        alertname: state.alertname.to_string(),
                                        alert_status: "firing".to_string(),
                                        severity: state.severity.clone(),
                                        summary: state.summary.clone(),
                                        description: state.description.clone(),
                                        value: input.current_value,
                                        threshold: input.threshold,
                                        starts_at: now_unix,
                                        ends_at: None,
                                        details: input.details,
                                    });
                                }
                            } else {
                                state.state = AlertState::Pending;
                                state.pending_since_mono = Some(now_mono);
                                state.pending_since = Some(now_unix);
                            }
                        }
                        AlertState::Pending => {
                            let elapsed = state
                                .pending_since_mono
                                .map(|since| now_mono.saturating_duration_since(since))
                                .unwrap_or_default();
                            if elapsed >= Duration::from_secs(input.for_secs) {
                                state.state = AlertState::Firing;
                                state.active_at = Some(now_unix);
                                state.last_notified_mono = Some(now_mono);
                                state.last_notified_at = Some(now_unix);
                                self.metrics.record_alert_fired(state.rule);
                                self.logger.log_security(
                                    Severity::Warning,
                                    "alert.firing",
                                    "firing",
                                    Some("system"),
                                    None,
                                    None,
                                    format!(
                                        "{} firing ({}): {}",
                                        state.alertname, state.severity, state.description
                                    ),
                                );
                                if notify {
                                    pending_notifications.push(PendingAlertNotification {
                                        rule: state.rule.to_string(),
                                        alertname: state.alertname.to_string(),
                                        alert_status: "firing".to_string(),
                                        severity: state.severity.clone(),
                                        summary: state.summary.clone(),
                                        description: state.description.clone(),
                                        value: input.current_value,
                                        threshold: input.threshold,
                                        starts_at: state.active_at.unwrap_or(now_unix),
                                        ends_at: None,
                                        details: input.details,
                                    });
                                }
                            }
                        }
                        AlertState::Firing => {
                            let should_repeat = cfg.repeat_interval_secs > 0
                                && state
                                    .last_notified_mono
                                    .map(|last| {
                                        now_mono.saturating_duration_since(last)
                                            >= Duration::from_secs(cfg.repeat_interval_secs)
                                    })
                                    .unwrap_or(true);
                            if should_repeat && notify {
                                state.last_notified_mono = Some(now_mono);
                                state.last_notified_at = Some(now_unix);
                                pending_notifications.push(PendingAlertNotification {
                                    rule: state.rule.to_string(),
                                    alertname: state.alertname.to_string(),
                                    alert_status: "firing".to_string(),
                                    severity: state.severity.clone(),
                                    summary: state.summary.clone(),
                                    description: state.description.clone(),
                                    value: input.current_value,
                                    threshold: input.threshold,
                                    starts_at: state.active_at.unwrap_or(now_unix),
                                    ends_at: None,
                                    details: input.details,
                                });
                            }
                        }
                    }
                } else {
                    match state.state {
                        AlertState::Inactive => {}
                        AlertState::Pending => {
                            state.state = AlertState::Inactive;
                            state.pending_since_mono = None;
                            state.pending_since = None;
                        }
                        AlertState::Firing => {
                            let starts_at = state.active_at.unwrap_or(now_unix);
                            state.state = AlertState::Inactive;
                            state.pending_since_mono = None;
                            state.pending_since = None;
                            state.active_at = None;
                            state.resolved_at = Some(now_unix);
                            self.metrics.record_alert_resolved(state.rule);
                            self.logger.log_security(
                                Severity::Info,
                                "alert.resolved",
                                "resolved",
                                Some("system"),
                                None,
                                None,
                                format!("{} resolved: {}", state.alertname, state.description),
                            );
                            if notify && cfg.send_resolved {
                                state.last_notified_mono = Some(now_mono);
                                state.last_notified_at = Some(now_unix);
                                pending_notifications.push(PendingAlertNotification {
                                    rule: state.rule.to_string(),
                                    alertname: state.alertname.to_string(),
                                    alert_status: "resolved".to_string(),
                                    severity: state.severity.clone(),
                                    summary: state.summary.clone(),
                                    description: state.description.clone(),
                                    value: input.current_value,
                                    threshold: input.threshold,
                                    starts_at,
                                    ends_at: Some(now_unix),
                                    details: input.details,
                                });
                            }
                        }
                    }
                }
            }
        }
        pending_notifications
    }

    pub async fn dispatch_notifications(
        &self,
        notifications: Vec<PendingAlertNotification>,
    ) -> Vec<AlertDeliveryRecord> {
        if notifications.is_empty() {
            return Vec::new();
        }
        let cfg = self.config();
        let mut results = Vec::new();
        for notif in notifications {
            let now = unix_timestamp();
            let delivery_id = format!("alert_{}", Uuid::new_v4().simple());
            let payload = build_alert_webhook_payload(&delivery_id, now, &notif);

            if let Some(url) = cfg.webhook_url.as_deref() {
                let rec = self
                    .send_direct_webhook(
                        url,
                        cfg.webhook_secret.as_deref(),
                        &delivery_id,
                        &notif,
                        &payload,
                        now,
                    )
                    .await;
                self.record_delivery(rec.clone());
                results.push(rec);
            }

            match self.webhooks.enqueue_alert(&payload) {
                Ok(queued) if queued > 0 => {
                    let rec = AlertDeliveryRecord {
                        delivery_id: format!("{delivery_id}_principals"),
                        target: format!("principal_webhooks({queued})"),
                        event_type: "alert".to_string(),
                        rule: notif.rule.clone(),
                        alertname: notif.alertname.clone(),
                        alert_status: notif.alert_status.clone(),
                        status: "queued".to_string(),
                        http_status: None,
                        error: None,
                        created_at: now,
                    };
                    self.record_delivery(rec.clone());
                    results.push(rec);
                }
                Ok(_) => {}
                Err(err) => {
                    self.logger.log_error(
                        "alert.principal_webhook_enqueue_failed",
                        format!("failed to enqueue principal alert webhooks: {err:#}"),
                    );
                }
            }
        }
        results
    }

    pub async fn send_test_alert(&self, actor: &str) -> Result<Vec<AlertDeliveryRecord>> {
        let cfg = self.config();
        let now = unix_timestamp();
        let delivery_id = format!("alert_test_{}", Uuid::new_v4().simple());
        let notif = PendingAlertNotification {
            rule: "connection_test".to_string(),
            alertname: "AircdAlertWebhookTest".to_string(),
            alert_status: "test".to_string(),
            severity: "info".to_string(),
            summary: format!("Prometheus alert webhook connectivity test triggered by {actor}"),
            description: "Synthetic test alert from aircd alert evaluator.".to_string(),
            value: 1.0,
            threshold: 1.0,
            starts_at: now,
            ends_at: None,
            details: json!({ "triggered_by": actor }),
        };
        let payload = build_alert_webhook_payload(&delivery_id, now, &notif);

        let mut results = Vec::new();
        let mut direct_err: Option<String> = None;

        if let Some(url) = cfg.webhook_url.as_deref() {
            let rec = self
                .send_direct_webhook(
                    url,
                    cfg.webhook_secret.as_deref(),
                    &delivery_id,
                    &notif,
                    &payload,
                    now,
                )
                .await;
            if rec.status == "failed" {
                direct_err = rec.error.clone();
            }
            self.record_delivery(rec.clone());
            results.push(rec);
        }

        let queued = self.webhooks.enqueue_alert(&payload)?;
        if queued > 0 {
            let rec = AlertDeliveryRecord {
                delivery_id: format!("{delivery_id}_principals"),
                target: format!("principal_webhooks({queued})"),
                event_type: "alert".to_string(),
                rule: notif.rule,
                alertname: notif.alertname,
                alert_status: notif.alert_status,
                status: "queued".to_string(),
                http_status: None,
                error: None,
                created_at: now,
            };
            self.record_delivery(rec.clone());
            results.push(rec);
        }

        if let Some(err) = direct_err {
            return Err(anyhow!("{err}"));
        }
        if results.is_empty() {
            bail!(
                "No alert webhook configured: set [alerts].webhook_url / --alerts-webhook-url or register a principal webhook subscribed to 'alert'"
            );
        }
        Ok(results)
    }

    async fn send_direct_webhook(
        &self,
        url: &str,
        secret: Option<&str>,
        delivery_id: &str,
        notif: &PendingAlertNotification,
        payload: &serde_json::Value,
        now: i64,
    ) -> AlertDeliveryRecord {
        let body = match serde_json::to_string(payload) {
            Ok(b) => b,
            Err(err) => {
                self.metrics.record_alert_webhook_delivery(false);
                return AlertDeliveryRecord {
                    delivery_id: delivery_id.to_string(),
                    target: "server_webhook".to_string(),
                    event_type: "alert".to_string(),
                    rule: notif.rule.clone(),
                    alertname: notif.alertname.clone(),
                    alert_status: notif.alert_status.clone(),
                    status: "failed".to_string(),
                    http_status: None,
                    error: Some(format!("serialize alert payload: {err}")),
                    created_at: now,
                };
            }
        };

        let mut req = self
            .http_client
            .post(url)
            .header("Content-Type", "application/json; charset=utf-8")
            .header("X-AirCD-Event", "alert")
            .header("X-AirCD-Delivery", delivery_id)
            .header("X-Agent-Ping-Event", "alert")
            .header("X-Agent-Ping-Delivery", delivery_id);

        if let Some(secret) = secret {
            let sig = sign_payload(secret, body.as_bytes());
            req = req
                .header("X-AirCD-Signature-256", &sig)
                .header("X-Agent-Ping-Signature-256", &sig);
        }

        match req.body(body).send().await {
            Ok(resp) => {
                let code = resp.status().as_u16() as i64;
                if (200..300).contains(&code) {
                    self.metrics.record_alert_webhook_delivery(true);
                    AlertDeliveryRecord {
                        delivery_id: delivery_id.to_string(),
                        target: "server_webhook".to_string(),
                        event_type: "alert".to_string(),
                        rule: notif.rule.clone(),
                        alertname: notif.alertname.clone(),
                        alert_status: notif.alert_status.clone(),
                        status: "delivered".to_string(),
                        http_status: Some(code),
                        error: None,
                        created_at: now,
                    }
                } else {
                    let err_msg = format!("alert webhook returned HTTP {code}");
                    self.metrics.record_alert_webhook_delivery(false);
                    self.logger.log_error(
                        "alert.webhook_failed",
                        format!("{} ({}): {err_msg}", notif.alertname, notif.alert_status),
                    );
                    AlertDeliveryRecord {
                        delivery_id: delivery_id.to_string(),
                        target: "server_webhook".to_string(),
                        event_type: "alert".to_string(),
                        rule: notif.rule.clone(),
                        alertname: notif.alertname.clone(),
                        alert_status: notif.alert_status.clone(),
                        status: "failed".to_string(),
                        http_status: Some(code),
                        error: Some(err_msg),
                        created_at: now,
                    }
                }
            }
            Err(err) => {
                let err_msg = format!("alert webhook request failed: {err}");
                self.metrics.record_alert_webhook_delivery(false);
                self.logger.log_error(
                    "alert.webhook_failed",
                    format!("{} ({}): {err_msg}", notif.alertname, notif.alert_status),
                );
                AlertDeliveryRecord {
                    delivery_id: delivery_id.to_string(),
                    target: "server_webhook".to_string(),
                    event_type: "alert".to_string(),
                    rule: notif.rule.clone(),
                    alertname: notif.alertname.clone(),
                    alert_status: notif.alert_status.clone(),
                    status: "failed".to_string(),
                    http_status: None,
                    error: Some(err_msg),
                    created_at: now,
                }
            }
        }
    }

    fn record_delivery(&self, record: AlertDeliveryRecord) {
        let mut guard = self.inner.write().expect("alert delivery lock poisoned");
        guard.deliveries.push_front(record);
        while guard.deliveries.len() > MAX_ALERT_DELIVERY_HISTORY {
            guard.deliveries.pop_back();
        }
    }
}

fn build_alert_webhook_payload(
    delivery_id: &str,
    now: i64,
    notif: &PendingAlertNotification,
) -> serde_json::Value {
    let status_tag = notif.alert_status.to_ascii_uppercase();
    let message = format!(
        "[{status_tag}:{}] {}: {}",
        notif.severity, notif.alertname, notif.description
    );
    let labels = json!({
        "alertname": notif.alertname,
        "rule": notif.rule,
        "severity": notif.severity,
        "service": "aircd",
    });
    let annotations = json!({
        "summary": notif.summary,
        "description": notif.description,
    });
    json!({
        "delivery_id": delivery_id,
        "event": "alert",
        "status": notif.alert_status,
        "channel": "#alerts",
        "from": "aircd",
        "to": null,
        "alertname": notif.alertname,
        "rule": notif.rule,
        "severity": notif.severity,
        "message": message,
        "summary": notif.summary,
        "description": notif.description,
        "value": notif.value,
        "threshold": notif.threshold,
        "starts_at": notif.starts_at,
        "ends_at": notif.ends_at,
        "created_at": now,
        "labels": labels,
        "annotations": annotations,
        "details": notif.details,
        "alerts": [{
            "status": notif.alert_status,
            "labels": labels,
            "annotations": annotations,
            "startsAt": notif.starts_at,
            "endsAt": notif.ends_at,
        }],
    })
}

fn build_rule_inputs(cfg: &AlertConfig, snap: &AlertEvalSnapshot) -> [RuleEvaluationInput; 3] {
    // 1. Sustained high wake failure rate
    let wf = &snap.wake_failure;
    let wake_condition = cfg.wake_failure_enabled
        && wf.total_completed >= cfg.wake_failure_min_samples
        && if cfg.wake_failure_rate_threshold > 0.0 {
            wf.failure_rate >= cfg.wake_failure_rate_threshold
        } else {
            wf.failure_rate > 0.0
        };
    let wake_op = if cfg.wake_failure_rate_threshold > 0.0 {
        ">="
    } else {
        ">"
    };
    let wake_base_expr = format!(
        "((sum(increase(aircd_wake_runs_total{{status=~\"failed|timeout\"}}[{}s])) or vector(0)) / clamp_min(sum(increase(aircd_wake_runs_total{{status=~\"ok|failed|timeout\"}}[{}s])), 1)) {wake_op} {}",
        cfg.wake_failure_window_secs,
        cfg.wake_failure_window_secs,
        cfg.wake_failure_rate_threshold,
    );
    let wake_expr = if cfg.wake_failure_min_samples > 0 {
        format!(
            "{wake_base_expr} and sum(increase(aircd_wake_runs_total{{status=~\"ok|failed|timeout\"}}[{}s])) >= {}",
            cfg.wake_failure_window_secs, cfg.wake_failure_min_samples
        )
    } else {
        format!(
            "{wake_base_expr} and sum(increase(aircd_wake_runs_total{{status=~\"ok|failed|timeout\"}}[{}s])) > 0",
            cfg.wake_failure_window_secs
        )
    };
    let wake_desc = format!(
        "Wake failure rate is {:.1}% ({} failures / {} completed in {}s; threshold {:.1}%, min samples {}).",
        wf.failure_rate * 100.0,
        wf.failures,
        wf.total_completed,
        cfg.wake_failure_window_secs,
        cfg.wake_failure_rate_threshold * 100.0,
        cfg.wake_failure_min_samples,
    );

    // 2. Queue backlog
    let queue_threshold = cfg.queue_backlog_threshold.max(1);
    let queue_condition = cfg.queue_backlog_enabled && snap.queue_backlog_total >= queue_threshold;
    let queue_expr = format!(
        "((aircd_active_sessions{{kind=\"wake_pending\"}} or vector(0)) + (aircd_webhook_deliveries_current{{status=\"pending\"}} or vector(0))) >= {queue_threshold}"
    );
    let queue_desc = format!(
        "Queue backlog is {} items (wake_pending={}, webhook_pending={}; threshold {}).",
        snap.queue_backlog_total, snap.wake_pending, snap.webhook_pending, queue_threshold
    );

    // 3. Disk pressure
    let (disk_condition, disk_value, disk_threshold, disk_desc, disk_details) = match &snap.disk {
        Some(disk) => {
            let ratio_breach = cfg.disk_used_ratio_threshold > 0.0
                && disk.used_ratio >= cfg.disk_used_ratio_threshold;
            let bytes_breach =
                cfg.disk_min_free_bytes > 0 && disk.available_bytes < cfg.disk_min_free_bytes;
            let cond = cfg.disk_pressure_enabled && (ratio_breach || bytes_breach);
            let value = if cfg.disk_used_ratio_threshold > 0.0 {
                disk.used_ratio
            } else {
                disk.available_bytes as f64
            };
            let thresh = if cfg.disk_used_ratio_threshold > 0.0 {
                cfg.disk_used_ratio_threshold
            } else {
                cfg.disk_min_free_bytes as f64
            };
            let desc = format!(
                "Disk usage at '{}' is {:.1}% ({} bytes available / {} total; ratio threshold {:.1}%, min free {} bytes).",
                disk.path,
                disk.used_ratio * 100.0,
                disk.available_bytes,
                disk.total_bytes,
                cfg.disk_used_ratio_threshold * 100.0,
                cfg.disk_min_free_bytes,
            );
            let details = json!({
                "path": disk.path,
                "total_bytes": disk.total_bytes,
                "available_bytes": disk.available_bytes,
                "free_bytes": disk.free_bytes,
                "used_bytes": disk.used_bytes,
                "used_ratio": disk.used_ratio,
                "ratio_breach": ratio_breach,
                "bytes_breach": bytes_breach,
            });
            (cond, value, thresh, desc, details)
        }
        None => (
            false,
            0.0,
            cfg.disk_used_ratio_threshold,
            "Disk pressure check skipped (in-memory database with no disk_path configured)."
                .to_string(),
            json!({ "available": false }),
        ),
    };
    let disk_expr = match (
        cfg.disk_used_ratio_threshold > 0.0,
        cfg.disk_min_free_bytes > 0,
    ) {
        (true, true) => format!(
            "(aircd_disk_used_ratio >= {}) or (aircd_disk_available_bytes < {})",
            cfg.disk_used_ratio_threshold, cfg.disk_min_free_bytes
        ),
        (true, false) => format!("aircd_disk_used_ratio >= {}", cfg.disk_used_ratio_threshold),
        (false, true) => format!("aircd_disk_available_bytes < {}", cfg.disk_min_free_bytes),
        (false, false) => "vector(0) > 1".to_string(),
    };

    [
        RuleEvaluationInput {
            rule: "wake_failure_rate",
            alertname: "AircdHighWakeFailureRate",
            enabled: cfg.wake_failure_enabled,
            severity: cfg.wake_failure_severity.clone(),
            for_secs: cfg.wake_failure_for_secs,
            condition_met: wake_condition,
            current_value: wf.failure_rate,
            threshold: cfg.wake_failure_rate_threshold,
            expr: wake_expr,
            summary: "Sustained high wake failure rate in aircd".to_string(),
            description: wake_desc,
            details: json!({
                "window_secs": cfg.wake_failure_window_secs,
                "total_completed": wf.total_completed,
                "ok": wf.ok,
                "failed": wf.failed,
                "timeout": wf.timeout,
                "failures": wf.failures,
                "failure_rate": wf.failure_rate,
                "min_samples": cfg.wake_failure_min_samples,
            }),
        },
        RuleEvaluationInput {
            rule: "queue_backlog",
            alertname: "AircdQueueBacklogHigh",
            enabled: cfg.queue_backlog_enabled,
            severity: cfg.queue_backlog_severity.clone(),
            for_secs: cfg.queue_backlog_for_secs,
            condition_met: queue_condition,
            current_value: snap.queue_backlog_total as f64,
            threshold: queue_threshold as f64,
            expr: queue_expr,
            summary: "Sustained queue backlog in aircd".to_string(),
            description: queue_desc,
            details: json!({
                "wake_pending": snap.wake_pending,
                "webhook_pending": snap.webhook_pending,
                "total_backlog": snap.queue_backlog_total,
            }),
        },
        RuleEvaluationInput {
            rule: "disk_pressure",
            alertname: "AircdDiskPressureHigh",
            enabled: cfg.disk_pressure_enabled,
            severity: cfg.disk_pressure_severity.clone(),
            for_secs: cfg.disk_pressure_for_secs,
            condition_met: disk_condition,
            current_value: disk_value,
            threshold: disk_threshold,
            expr: disk_expr,
            summary: "Disk pressure on aircd storage volume".to_string(),
            description: disk_desc,
            details: disk_details,
        },
    ]
}

fn rule_template_description(cfg: &AlertConfig, rule: &str) -> String {
    match rule {
        "wake_failure_rate" => format!(
            "Wake failure rate exceeded {:.1}% over {}s (min {} samples) for {}.",
            cfg.wake_failure_rate_threshold * 100.0,
            cfg.wake_failure_window_secs,
            cfg.wake_failure_min_samples,
            format_prom_duration(cfg.wake_failure_for_secs),
        ),
        "queue_backlog" => format!(
            "Pending wake and webhook deliveries remained >= {} for {}.",
            cfg.queue_backlog_threshold.max(1),
            format_prom_duration(cfg.queue_backlog_for_secs),
        ),
        "disk_pressure" => format!(
            "Volume usage exceeded {:.1}% or free space dropped below {} bytes for {}.",
            cfg.disk_used_ratio_threshold * 100.0,
            cfg.disk_min_free_bytes,
            format_prom_duration(cfg.disk_pressure_for_secs),
        ),
        _ => String::new(),
    }
}

pub fn format_prom_duration(secs: u64) -> String {
    if secs == 0 {
        return "0s".to_string();
    }
    if secs % 3600 == 0 {
        return format!("{}h", secs / 3600);
    }
    if secs % 60 == 0 {
        return format!("{}m", secs / 60);
    }
    let m = secs / 60;
    let s = secs % 60;
    if m > 0 {
        format!("{m}m{s}s")
    } else {
        format!("{s}s")
    }
}

fn yaml_scalar(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn resolve_existing_dir(path: &Path) -> PathBuf {
    for ancestor in path.ancestors() {
        if !ancestor.as_os_str().is_empty() && ancestor.is_dir() {
            return ancestor.to_path_buf();
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

pub fn sample_disk_stats(path: &Path) -> Option<DiskStats> {
    let dir = resolve_existing_dir(path);
    sample_disk_stats_dir(&dir)
}

#[cfg(windows)]
fn sample_disk_stats_dir(dir: &Path) -> Option<DiskStats> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    extern "system" {
        fn GetDiskFreeSpaceExW(
            lpDirectoryName: *const u16,
            lpFreeBytesAvailableToCaller: *mut u64,
            lpTotalNumberOfBytes: *mut u64,
            lpTotalNumberOfFreeBytes: *mut u64,
        ) -> i32;
    }

    let wide: Vec<u16> = dir
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut available = 0u64;
    let mut total = 0u64;
    let mut free = 0u64;
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut available,
            &mut total,
            &mut free,
        )
    };
    if ok == 0 {
        return None;
    }
    let used = total.saturating_sub(available);
    let used_ratio = if total > 0 {
        (used as f64 / total as f64).clamp(0.0, 1.0)
    } else {
        0.0
    };
    Some(DiskStats {
        path: dir.display().to_string(),
        total_bytes: total,
        available_bytes: available,
        free_bytes: free,
        used_bytes: used,
        used_ratio,
    })
}

#[cfg(not(windows))]
fn sample_disk_stats_dir(dir: &Path) -> Option<DiskStats> {
    let output = std::process::Command::new("df")
        .arg("-Pk")
        .arg(dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let data_line = text.lines().last()?;
    let cols: Vec<&str> = data_line.split_whitespace().collect();
    if cols.len() < 4 {
        return None;
    }
    // POSIX `df -Pk`: Filesystem 1024-blocks Used Available Capacity Mounted-on
    let total_kib = cols[1].parse::<u64>().ok()?;
    let used_kib = cols[2].parse::<u64>().ok()?;
    let avail_kib = cols[3].parse::<u64>().ok()?;
    let total_bytes = total_kib.saturating_mul(1024);
    let available_bytes = avail_kib.saturating_mul(1024);
    let used_bytes = used_kib.saturating_mul(1024);
    let used_ratio = if total_bytes > 0 {
        (total_bytes.saturating_sub(available_bytes) as f64 / total_bytes as f64).clamp(0.0, 1.0)
    } else {
        0.0
    };
    Some(DiskStats {
        path: dir.display().to_string(),
        total_bytes,
        available_bytes,
        free_bytes: available_bytes,
        used_bytes,
        used_ratio,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store;

    fn test_manager(cfg: AlertConfig) -> (AlertManager, Arc<MetricsRegistry>) {
        let db = store::open(":memory:").unwrap();
        let logger = EventForwarder::default();
        let metrics = Arc::new(MetricsRegistry::new());
        let webhooks = WebhookStore::with_logger(db, logger.clone());
        let manager = AlertManager::new(cfg, None, logger, metrics.clone(), webhooks);
        (manager, metrics)
    }

    #[test]
    fn sustained_for_duration_gates_firing_and_emits_resolved() {
        let mut cfg = AlertConfig::default();
        cfg.wake_failure_for_secs = 60;
        cfg.wake_failure_rate_threshold = 0.25;
        cfg.wake_failure_min_samples = 4;
        cfg.repeat_interval_secs = 300;
        let (mgr, metrics) = test_manager(cfg);

        let t0 = Instant::now();
        let high_failure = AlertEvalSnapshot {
            wake_failure: WakeFailureWindowStats {
                window_secs: 300,
                total_completed: 4,
                ok: 2,
                failed: 2,
                timeout: 0,
                failures: 2,
                failure_rate: 0.5,
            },
            ..Default::default()
        };

        // First breach -> enters Pending, no notification yet.
        let n1 = mgr.evaluate_with_snapshot(t0, 1000, &high_failure, true);
        assert!(n1.is_empty());
        assert_eq!(mgr.status(&high_failure).rules[0].state, AlertState::Pending);

        // Still under for_secs (30s < 60s) -> remains Pending, no notification.
        let n2 = mgr.evaluate_with_snapshot(t0 + Duration::from_secs(30), 1030, &high_failure, true);
        assert!(n2.is_empty());
        assert_eq!(mgr.status(&high_failure).rules[0].state, AlertState::Pending);

        // Sustained >= 60s -> transitions to Firing and emits notification.
        let n3 = mgr.evaluate_with_snapshot(t0 + Duration::from_secs(60), 1060, &high_failure, true);
        assert_eq!(n3.len(), 1);
        assert_eq!(n3[0].rule, "wake_failure_rate");
        assert_eq!(n3[0].alertname, "AircdHighWakeFailureRate");
        assert_eq!(n3[0].alert_status, "firing");
        assert_eq!(mgr.status(&high_failure).rules[0].state, AlertState::Firing);

        // Still firing before repeat_interval (120s < 300s) -> no duplicate notification.
        let n4 = mgr.evaluate_with_snapshot(t0 + Duration::from_secs(120), 1120, &high_failure, true);
        assert!(n4.is_empty());

        // After repeat_interval (360s - 60s = 300s) -> repeats firing notification.
        let n5 = mgr.evaluate_with_snapshot(t0 + Duration::from_secs(360), 1360, &high_failure, true);
        assert_eq!(n5.len(), 1);
        assert_eq!(n5[0].alert_status, "firing");

        // Recovery -> transitions to Inactive and emits resolved notification.
        let healthy = AlertEvalSnapshot {
            wake_failure: WakeFailureWindowStats {
                window_secs: 300,
                total_completed: 10,
                ok: 9,
                failed: 1,
                timeout: 0,
                failures: 1,
                failure_rate: 0.1,
            },
            ..Default::default()
        };
        let n6 = mgr.evaluate_with_snapshot(t0 + Duration::from_secs(380), 1380, &healthy, true);
        assert_eq!(n6.len(), 1);
        assert_eq!(n6[0].alert_status, "resolved");
        assert_eq!(mgr.status(&healthy).rules[0].state, AlertState::Inactive);

        let scraped = metrics.render_prometheus(&Default::default());
        assert!(scraped.contains("aircd_alerts_fired_total{rule=\"wake_failure_rate\"} 1\n"));
        assert!(scraped.contains("aircd_alerts_resolved_total{rule=\"wake_failure_rate\"} 1\n"));
    }

    #[test]
    fn transient_spike_before_for_secs_resets_without_firing() {
        let mut cfg = AlertConfig::default();
        cfg.queue_backlog_threshold = 10;
        cfg.queue_backlog_for_secs = 60;
        let (mgr, _) = test_manager(cfg);

        let t0 = Instant::now();
        let spike = AlertEvalSnapshot {
            wake_pending: 8,
            webhook_pending: 4,
            queue_backlog_total: 12,
            ..Default::default()
        };
        assert!(mgr.evaluate_with_snapshot(t0, 1000, &spike, true).is_empty());
        assert_eq!(mgr.status(&spike).rules[1].state, AlertState::Pending);

        // Backlog drains at 40s (< 60s) -> returns to Inactive without firing or resolving.
        let cleared = AlertEvalSnapshot {
            wake_pending: 1,
            webhook_pending: 1,
            queue_backlog_total: 2,
            ..Default::default()
        };
        let n = mgr.evaluate_with_snapshot(t0 + Duration::from_secs(40), 1040, &cleared, true);
        assert!(n.is_empty());
        assert_eq!(mgr.status(&cleared).rules[1].state, AlertState::Inactive);
    }

    #[test]
    fn disk_pressure_triggers_on_ratio_or_min_free_bytes() {
        let mut cfg = AlertConfig::default();
        cfg.disk_used_ratio_threshold = 0.85;
        cfg.disk_min_free_bytes = 500_000_000;
        cfg.disk_pressure_for_secs = 0;
        let (mgr, _) = test_manager(cfg);

        let t0 = Instant::now();
        let ok_disk = AlertEvalSnapshot {
            disk: Some(DiskStats {
                path: "/data".into(),
                total_bytes: 10_000_000_000,
                available_bytes: 2_000_000_000,
                free_bytes: 2_000_000_000,
                used_bytes: 8_000_000_000,
                used_ratio: 0.80,
            }),
            ..Default::default()
        };
        assert!(mgr.evaluate_with_snapshot(t0, 1000, &ok_disk, true).is_empty());

        // Min free bytes breach even though ratio is below 0.85.
        let low_bytes = AlertEvalSnapshot {
            disk: Some(DiskStats {
                path: "/data".into(),
                total_bytes: 2_000_000_000,
                available_bytes: 400_000_000,
                free_bytes: 400_000_000,
                used_bytes: 1_600_000_000,
                used_ratio: 0.80,
            }),
            ..Default::default()
        };
        let fired = mgr.evaluate_with_snapshot(t0 + Duration::from_secs(10), 1010, &low_bytes, true);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].rule, "disk_pressure");
        assert_eq!(fired[0].alertname, "AircdDiskPressureHigh");
    }

    #[test]
    fn prometheus_rules_yaml_exports_configured_thresholds() {
        let mut cfg = AlertConfig::default();
        cfg.group_name = "cams_prod".into();
        cfg.eval_interval_secs = 30;
        cfg.wake_failure_rate_threshold = 0.4;
        cfg.wake_failure_for_secs = 180;
        cfg.queue_backlog_enabled = false;
        let (mgr, _) = test_manager(cfg);

        let yaml_enabled_only = mgr.export_prometheus_rules_yaml(false);
        assert!(yaml_enabled_only.contains("name: \"cams_prod\""));
        assert!(yaml_enabled_only.contains("interval: \"30s\""));
        assert!(yaml_enabled_only.contains("alert: \"AircdHighWakeFailureRate\""));
        assert!(yaml_enabled_only.contains("for: \"3m\""));
        assert!(yaml_enabled_only.contains(">= 0.4"));
        assert!(!yaml_enabled_only.contains("AircdQueueBacklogHigh"));
        assert!(yaml_enabled_only.contains("AircdDiskPressureHigh"));

        let yaml_all = mgr.export_prometheus_rules_yaml(true);
        assert!(yaml_all.contains("AircdQueueBacklogHigh"));
    }

    #[test]
    fn sample_disk_stats_reads_current_volume() {
        let dir = std::env::temp_dir();
        let stats = sample_disk_stats(&dir).expect("disk stats on temp dir");
        assert!(stats.total_bytes > 0);
        assert!(stats.used_ratio >= 0.0 && stats.used_ratio <= 1.0);
    }
}
