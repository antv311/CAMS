//! In-memory Prometheus metrics registry and text exposition (format 0.0.4).

use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::db::{DbPageStats, PoolStats};
use crate::tasks::{ALL_STATUSES, TERMINAL_STATUSES};

pub const PROMETHEUS_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

const HTTP_DURATION_BUCKETS: &[f64] = &[
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 5.0,
];

const WAKE_DISPATCH_BUCKETS: &[f64] = &[
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0,
];

const WAKE_EXECUTION_BUCKETS: &[f64] = &[
    0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1800.0,
];

const TASK_KINDS: &[&str] = &["task", "bug", "feature"];
const WAKE_OUTCOMES: &[&str] = &["ok", "failed", "timeout"];
const WAKE_RUN_STATUSES: &[&str] = &["ok", "failed", "timeout", "skipped", "detached"];
const WAKE_REQUEST_RESULTS: &[&str] = &["started", "queued", "coalesced", "skipped", "disabled"];
const IRC_SESSION_EVENTS: &[&str] = &[
    "connected",
    "disconnected",
    "replaced",
    "revoked",
    "auth_failed",
];
const QUESTION_ACTIONS: &[&str] = &["asked", "answered", "escalated", "escalated_pm"];
const RATE_LIMIT_SCOPES: &[&str] = &["ip", "principal"];
pub const ALERT_RULE_KEYS: &[&str] = &["wake_failure_rate", "queue_backlog", "disk_pressure"];
const ALERT_DELIVERY_STATUSES: &[&str] = &["delivered", "failed"];
const AUDIT_STREAM_CATEGORIES: &[&str] = &["auth", "security", "task_state", "server_error"];
const TLS_RELOAD_RESULTS: &[(&str, &str)] = &[
    ("file_watcher", "ok"),
    ("file_watcher", "error"),
    ("sighup", "ok"),
    ("sighup", "error"),
    ("owner_api", "ok"),
    ("owner_api", "error"),
];
const MAX_WAKE_COMPLETION_HISTORY: usize = 50_000;
const MAX_WAKE_COMPLETION_AGE: Duration = Duration::from_secs(86_400);

#[derive(Debug)]
struct HistogramState {
    bounds: &'static [f64],
    /// Non-cumulative per-bucket counts; rendered cumulatively at scrape time.
    bucket_counts: Vec<u64>,
    count: u64,
    sum: f64,
}

impl HistogramState {
    fn new(bounds: &'static [f64]) -> Self {
        Self {
            bounds,
            bucket_counts: vec![0; bounds.len()],
            count: 0,
            sum: 0.0,
        }
    }

    fn observe(&mut self, value: f64) {
        let sample = if value.is_finite() && value >= 0.0 {
            value
        } else {
            return;
        };
        self.count += 1;
        self.sum += sample;
        for (idx, bound) in self.bounds.iter().enumerate() {
            if sample <= *bound {
                self.bucket_counts[idx] += 1;
                break;
            }
        }
    }

    fn snapshot(&self) -> HistogramSnapshot {
        let mut cumulative = Vec::with_capacity(self.bounds.len());
        let mut running = 0u64;
        for (idx, bound) in self.bounds.iter().enumerate() {
            running += self.bucket_counts[idx];
            cumulative.push((*bound, running));
        }
        HistogramSnapshot {
            buckets: cumulative,
            count: self.count,
            sum: self.sum,
        }
    }
}

#[derive(Clone, Debug)]
pub struct HistogramSnapshot {
    pub buckets: Vec<(f64, u64)>,
    pub count: u64,
    pub sum: f64,
}

#[derive(Debug)]
struct Histogram {
    inner: Mutex<HistogramState>,
}

impl Histogram {
    fn new(bounds: &'static [f64]) -> Self {
        Self {
            inner: Mutex::new(HistogramState::new(bounds)),
        }
    }

    fn observe(&self, value: f64) {
        self.inner
            .lock()
            .expect("metrics histogram mutex poisoned")
            .observe(value);
    }

    fn snapshot(&self) -> HistogramSnapshot {
        self.inner
            .lock()
            .expect("metrics histogram mutex poisoned")
            .snapshot()
    }
}

/// Rolling-window wake outcome summary used by Prometheus alerting rules.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct WakeFailureWindowStats {
    pub window_secs: u64,
    pub total_completed: u64,
    pub ok: u64,
    pub failed: u64,
    pub timeout: u64,
    pub failures: u64,
    pub failure_rate: f64,
}

/// Filesystem capacity and pressure telemetry for the database volume.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DiskStats {
    pub path: String,
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub free_bytes: u64,
    pub used_bytes: u64,
    pub used_ratio: f64,
}

/// Per-rule alert state sample exported on `/metrics`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AlertMetricSample {
    pub rule: String,
    pub alertname: String,
    pub severity: String,
    pub state_code: u8,
    pub active: bool,
}

/// Live database and runtime gauges sampled on `/metrics` scrape.
#[derive(Clone, Debug, Default)]
pub struct MetricsSnapshot {
    pub irc_sessions: usize,
    pub wake_running_sessions: usize,
    pub wake_pending_sessions: usize,
    pub wake_stored_sessions: usize,
    pub wake_quota_cooldowns_active: usize,
    pub principals_total: u64,
    pub messages_total: u64,
    pub tasks_by_status: BTreeMap<String, u64>,
    pub task_events_by_action: BTreeMap<String, u64>,
    pub webhook_deliveries_by_status: BTreeMap<String, u64>,
    pub db_pool: Option<PoolStats>,
    pub db_pages: Option<DbPageStats>,
    pub disk: Option<DiskStats>,
    pub alert_states: Vec<AlertMetricSample>,
    pub draining: bool,
}

/// Central thread-safe Prometheus metrics store shared across server subsystems.
#[derive(Debug)]
pub struct MetricsRegistry {
    started_at: Instant,
    http_requests_in_flight: AtomicI64,
    http_requests_total: Mutex<BTreeMap<(String, &'static str), u64>>,
    http_request_duration: Histogram,
    http_rate_limited_total: BTreeMap<&'static str, AtomicU64>,
    irc_sessions_total: BTreeMap<&'static str, AtomicU64>,
    tasks_created_total: BTreeMap<&'static str, AtomicU64>,
    tasks_completed_total: BTreeMap<&'static str, AtomicU64>,
    task_transitions_total: Mutex<BTreeMap<(String, String), u64>>,
    task_questions_total: BTreeMap<&'static str, AtomicU64>,
    wake_requests_total: BTreeMap<&'static str, AtomicU64>,
    wake_runs_total: BTreeMap<&'static str, AtomicU64>,
    wake_completion_history: Mutex<VecDeque<(Instant, &'static str)>>,
    wake_dispatch_duration: Histogram,
    wake_duration: BTreeMap<&'static str, Histogram>,
    wake_end_to_end_duration: BTreeMap<&'static str, Histogram>,
    alerts_fired_total: BTreeMap<&'static str, AtomicU64>,
    alerts_resolved_total: BTreeMap<&'static str, AtomicU64>,
    alert_webhook_deliveries_total: BTreeMap<&'static str, AtomicU64>,
    shutdown_lease_checkpoints_total: AtomicU64,
    shutdown_detached_workers_total: AtomicU64,
    shutdown_cancelled_wakes_total: AtomicU64,
    audit_stream_events_total: BTreeMap<&'static str, AtomicU64>,
    audit_stream_active_subscribers: AtomicI64,
    audit_stream_lagged_total: AtomicU64,
    tls_reloads_total: BTreeMap<(&'static str, &'static str), AtomicU64>,
    tls_reload_generation: AtomicU64,
    tls_last_reload_timestamp_seconds: AtomicU64,
}

impl Default for MetricsRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl MetricsRegistry {
    pub fn new() -> Self {
        Self {
            started_at: Instant::now(),
            http_requests_in_flight: AtomicI64::new(0),
            http_requests_total: Mutex::new(BTreeMap::new()),
            http_request_duration: Histogram::new(HTTP_DURATION_BUCKETS),
            http_rate_limited_total: counter_map(RATE_LIMIT_SCOPES),
            irc_sessions_total: counter_map(IRC_SESSION_EVENTS),
            tasks_created_total: counter_map(TASK_KINDS),
            tasks_completed_total: counter_map(TERMINAL_STATUSES),
            task_transitions_total: Mutex::new(BTreeMap::new()),
            task_questions_total: counter_map(QUESTION_ACTIONS),
            wake_requests_total: counter_map(WAKE_REQUEST_RESULTS),
            wake_runs_total: counter_map(WAKE_RUN_STATUSES),
            wake_completion_history: Mutex::new(VecDeque::new()),
            wake_dispatch_duration: Histogram::new(WAKE_DISPATCH_BUCKETS),
            wake_duration: histogram_map(WAKE_OUTCOMES, WAKE_EXECUTION_BUCKETS),
            wake_end_to_end_duration: histogram_map(WAKE_OUTCOMES, WAKE_EXECUTION_BUCKETS),
            alerts_fired_total: counter_map(ALERT_RULE_KEYS),
            alerts_resolved_total: counter_map(ALERT_RULE_KEYS),
            alert_webhook_deliveries_total: counter_map(ALERT_DELIVERY_STATUSES),
            shutdown_lease_checkpoints_total: AtomicU64::new(0),
            shutdown_detached_workers_total: AtomicU64::new(0),
            shutdown_cancelled_wakes_total: AtomicU64::new(0),
            audit_stream_events_total: counter_map(AUDIT_STREAM_CATEGORIES),
            audit_stream_active_subscribers: AtomicI64::new(0),
            audit_stream_lagged_total: AtomicU64::new(0),
            tls_reloads_total: TLS_RELOAD_RESULTS
                .iter()
                .map(|&k| (k, AtomicU64::new(0)))
                .collect(),
            tls_reload_generation: AtomicU64::new(0),
            tls_last_reload_timestamp_seconds: AtomicU64::new(0),
        }
    }

    pub fn inc_http_in_flight(&self) -> HttpInFlightGuard<'_> {
        self.http_requests_in_flight.fetch_add(1, Ordering::Relaxed);
        HttpInFlightGuard { registry: self }
    }

    pub fn record_http_request(&self, method: &str, status_code: u16, duration_secs: f64) {
        let norm_method = match method {
            "GET" | "POST" | "PATCH" | "PUT" | "DELETE" | "HEAD" | "OPTIONS" => {
                method.to_string()
            }
            _ => "OTHER".to_string(),
        };
        let status_class = match status_code {
            100..=199 => "1xx",
            200..=299 => "2xx",
            300..=399 => "3xx",
            400..=499 => "4xx",
            500..=599 => "5xx",
            _ => "other",
        };
        let mut guard = self
            .http_requests_total
            .lock()
            .expect("metrics http requests mutex poisoned");
        *guard.entry((norm_method, status_class)).or_insert(0) += 1;
        drop(guard);
        self.http_request_duration.observe(duration_secs);
    }

    pub fn record_http_rate_limited(&self, scope: &str) {
        if let Some(counter) = self.http_rate_limited_total.get(scope) {
            counter.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn record_irc_session(&self, event: &str) {
        if let Some(counter) = self.irc_sessions_total.get(event) {
            counter.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn record_task_created(&self, kind: &str) {
        let key = if TASK_KINDS.contains(&kind) {
            kind
        } else {
            "task"
        };
        if let Some(counter) = self.tasks_created_total.get(key) {
            counter.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn record_task_transition(&self, from: &str, to: &str) {
        let mut guard = self
            .task_transitions_total
            .lock()
            .expect("metrics task transitions mutex poisoned");
        *guard
            .entry((from.to_string(), to.to_string()))
            .or_insert(0) += 1;
    }

    pub fn record_task_completed(&self, outcome: &str) {
        if let Some(counter) = self.tasks_completed_total.get(outcome) {
            counter.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn record_task_question(&self, action: &str) {
        if let Some(counter) = self.task_questions_total.get(action) {
            counter.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn record_wake_request(&self, result: &str) {
        if let Some(counter) = self.wake_requests_total.get(result) {
            counter.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn record_wake_run_skipped(&self) {
        if let Some(counter) = self.wake_runs_total.get("skipped") {
            counter.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn record_wake_run_detached(&self) {
        if let Some(counter) = self.wake_runs_total.get("detached") {
            counter.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn record_shutdown_drain(
        &self,
        cancelled_wakes: usize,
        detached_workers: usize,
        checkpointed_leases: usize,
    ) {
        self.shutdown_cancelled_wakes_total
            .fetch_add(cancelled_wakes as u64, Ordering::Relaxed);
        self.shutdown_detached_workers_total
            .fetch_add(detached_workers as u64, Ordering::Relaxed);
        self.shutdown_lease_checkpoints_total
            .fetch_add(checkpointed_leases as u64, Ordering::Relaxed);
    }

    pub fn record_wake_dispatch(&self, dispatch_secs: f64) {
        self.wake_dispatch_duration.observe(dispatch_secs);
    }

    pub fn record_wake_completion(&self, outcome: &str, exec_secs: f64, e2e_secs: f64) {
        if let Some(counter) = self.wake_runs_total.get(outcome) {
            counter.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(hist) = self.wake_duration.get(outcome) {
            hist.observe(exec_secs);
        }
        if let Some(hist) = self.wake_end_to_end_duration.get(outcome) {
            hist.observe(e2e_secs);
        }
        if let Some(&static_outcome) = WAKE_OUTCOMES.iter().find(|&&o| o == outcome) {
            self.record_wake_outcome_at(Instant::now(), static_outcome);
        }
    }

    pub(crate) fn record_wake_outcome_at(&self, at: Instant, outcome: &'static str) {
        let mut history = self
            .wake_completion_history
            .lock()
            .expect("metrics wake history mutex poisoned");
        history.push_back((at, outcome));
        while history.len() > MAX_WAKE_COMPLETION_HISTORY {
            history.pop_front();
        }
        while let Some(&(ts, _)) = history.front() {
            if at.saturating_duration_since(ts) > MAX_WAKE_COMPLETION_AGE {
                history.pop_front();
            } else {
                break;
            }
        }
    }

    pub fn wake_failure_stats(&self, window: Duration) -> WakeFailureWindowStats {
        self.wake_failure_stats_at(Instant::now(), window)
    }

    pub(crate) fn wake_failure_stats_at(
        &self,
        now: Instant,
        window: Duration,
    ) -> WakeFailureWindowStats {
        let history = self
            .wake_completion_history
            .lock()
            .expect("metrics wake history mutex poisoned");
        let mut ok = 0u64;
        let mut failed = 0u64;
        let mut timeout = 0u64;
        for &(ts, outcome) in history.iter().rev() {
            if now.saturating_duration_since(ts) > window {
                if ts <= now {
                    break;
                }
                continue;
            }
            match outcome {
                "ok" => ok += 1,
                "failed" => failed += 1,
                "timeout" => timeout += 1,
                _ => {}
            }
        }
        let failures = failed + timeout;
        let total_completed = ok + failures;
        let failure_rate = if total_completed > 0 {
            failures as f64 / total_completed as f64
        } else {
            0.0
        };
        WakeFailureWindowStats {
            window_secs: window.as_secs(),
            total_completed,
            ok,
            failed,
            timeout,
            failures,
            failure_rate,
        }
    }

    pub fn record_alert_fired(&self, rule: &str) {
        if let Some(counter) = self.alerts_fired_total.get(rule) {
            counter.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn record_alert_resolved(&self, rule: &str) {
        if let Some(counter) = self.alerts_resolved_total.get(rule) {
            counter.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn record_alert_webhook_delivery(&self, delivered: bool) {
        let key = if delivered { "delivered" } else { "failed" };
        if let Some(counter) = self.alert_webhook_deliveries_total.get(key) {
            counter.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn record_audit_event(&self, category: &str) {
        if let Some(counter) = self.audit_stream_events_total.get(category) {
            counter.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn record_audit_lagged(&self, skipped: u64) {
        self.audit_stream_lagged_total
            .fetch_add(skipped, Ordering::Relaxed);
    }

    pub fn subscribe_audit_guard(self: &std::sync::Arc<Self>) -> AuditStreamSubscriberGuard {
        self.audit_stream_active_subscribers
            .fetch_add(1, Ordering::Relaxed);
        AuditStreamSubscriberGuard {
            registry: self.clone(),
        }
    }

    pub fn set_tls_initial_state(&self, generation: u64, loaded_at_secs: i64) {
        self.tls_reload_generation
            .store(generation, Ordering::Relaxed);
        self.tls_last_reload_timestamp_seconds
            .store(loaded_at_secs.max(0) as u64, Ordering::Relaxed);
    }

    pub fn record_tls_reload(
        &self,
        trigger: &str,
        ok: bool,
        generation: u64,
        timestamp_secs: i64,
    ) {
        let norm_trigger = match trigger {
            "file_watcher" | "sighup" | "owner_api" => trigger,
            _ if trigger.starts_with("owner_api") => "owner_api",
            _ => "owner_api",
        };
        let result = if ok { "ok" } else { "error" };
        if let Some(counter) = self.tls_reloads_total.get(&(norm_trigger, result)) {
            counter.fetch_add(1, Ordering::Relaxed);
        }
        self.tls_reload_generation
            .store(generation, Ordering::Relaxed);
        if ok {
            self.tls_last_reload_timestamp_seconds
                .store(timestamp_secs.max(0) as u64, Ordering::Relaxed);
        }
    }

    /// Render all metrics in Prometheus text exposition format 0.0.4.
    pub fn render_prometheus(&self, snap: &MetricsSnapshot) -> String {
        let mut out = String::with_capacity(8192);

        // Process basics
        write_header(&mut out, "aircd_up", "gauge", "Whether the aircd hub is up.");
        write_sample(&mut out, "aircd_up", &[], 1u64);

        write_header(
            &mut out,
            "aircd_uptime_seconds",
            "gauge",
            "Seconds elapsed since the aircd server started.",
        );
        write_sample_f64(
            &mut out,
            "aircd_uptime_seconds",
            &[],
            self.started_at.elapsed().as_secs_f64(),
        );

        // 1. Active sessions
        write_header(
            &mut out,
            "aircd_active_sessions",
            "gauge",
            "Current number of active hub sessions by kind.",
        );
        write_sample(
            &mut out,
            "aircd_active_sessions",
            &[("kind", "irc")],
            snap.irc_sessions as u64,
        );
        write_sample(
            &mut out,
            "aircd_active_sessions",
            &[("kind", "wake_running")],
            snap.wake_running_sessions as u64,
        );
        write_sample(
            &mut out,
            "aircd_active_sessions",
            &[("kind", "wake_pending")],
            snap.wake_pending_sessions as u64,
        );
        write_sample(
            &mut out,
            "aircd_active_sessions",
            &[("kind", "wake_stored")],
            snap.wake_stored_sessions as u64,
        );

        write_header(
            &mut out,
            "aircd_irc_sessions_total",
            "counter",
            "Total IRC session lifecycle events since server start.",
        );
        for event in IRC_SESSION_EVENTS {
            let val = self.irc_sessions_total[event].load(Ordering::Relaxed);
            write_sample(&mut out, "aircd_irc_sessions_total", &[("event", event)], val);
        }

        write_header(
            &mut out,
            "aircd_principals_total",
            "gauge",
            "Total registered agent/owner principals.",
        );
        write_sample(&mut out, "aircd_principals_total", &[], snap.principals_total);

        write_header(
            &mut out,
            "aircd_messages_total",
            "counter",
            "Total messages stored in hub history.",
        );
        write_sample(&mut out, "aircd_messages_total", &[], snap.messages_total);

        // 2. Task throughput
        write_header(
            &mut out,
            "aircd_tasks_current",
            "gauge",
            "Current number of tasks in each workflow status.",
        );
        for status in ALL_STATUSES {
            let count = snap.tasks_by_status.get(*status).copied().unwrap_or(0);
            write_sample(&mut out, "aircd_tasks_current", &[("status", status)], count);
        }
        for (status, count) in &snap.tasks_by_status {
            if !ALL_STATUSES.contains(&status.as_str()) {
                write_sample(&mut out, "aircd_tasks_current", &[("status", status)], *count);
            }
        }

        write_header(
            &mut out,
            "aircd_tasks_created_total",
            "counter",
            "Total tasks created since server start by kind.",
        );
        for kind in TASK_KINDS {
            let val = self.tasks_created_total[kind].load(Ordering::Relaxed);
            write_sample(&mut out, "aircd_tasks_created_total", &[("kind", kind)], val);
        }

        write_header(
            &mut out,
            "aircd_tasks_completed_total",
            "counter",
            "Total tasks reaching a terminal status since server start.",
        );
        for outcome in TERMINAL_STATUSES {
            let val = self.tasks_completed_total[outcome].load(Ordering::Relaxed);
            write_sample(
                &mut out,
                "aircd_tasks_completed_total",
                &[("outcome", outcome)],
                val,
            );
        }

        {
            let transitions = self
                .task_transitions_total
                .lock()
                .expect("metrics task transitions mutex poisoned");
            write_header(
                &mut out,
                "aircd_task_transitions_total",
                "counter",
                "Total task status transitions since server start.",
            );
            for ((from, to), count) in transitions.iter() {
                write_sample(
                    &mut out,
                    "aircd_task_transitions_total",
                    &[("from", from), ("to", to)],
                    *count,
                );
            }
        }

        write_header(
            &mut out,
            "aircd_task_questions_total",
            "counter",
            "Total task question lifecycle actions since server start.",
        );
        for action in QUESTION_ACTIONS {
            let val = self.task_questions_total[action].load(Ordering::Relaxed);
            write_sample(
                &mut out,
                "aircd_task_questions_total",
                &[("action", action)],
                val,
            );
        }

        write_header(
            &mut out,
            "aircd_task_events_persisted_total",
            "counter",
            "Total persisted task audit events in SQLite by action.",
        );
        for (action, count) in &snap.task_events_by_action {
            write_sample(
                &mut out,
                "aircd_task_events_persisted_total",
                &[("action", action)],
                *count,
            );
        }

        // 3. Wake latencies & throughput
        write_header(
            &mut out,
            "aircd_wake_requests_total",
            "counter",
            "Total wake requests processed since server start by result.",
        );
        for result in WAKE_REQUEST_RESULTS {
            let val = self.wake_requests_total[result].load(Ordering::Relaxed);
            write_sample(
                &mut out,
                "aircd_wake_requests_total",
                &[("result", result)],
                val,
            );
        }

        write_header(
            &mut out,
            "aircd_wake_runs_total",
            "counter",
            "Total wake runs finished or skipped since server start by status.",
        );
        for status in WAKE_RUN_STATUSES {
            let val = self.wake_runs_total[status].load(Ordering::Relaxed);
            write_sample(
                &mut out,
                "aircd_wake_runs_total",
                &[("status", status)],
                val,
            );
        }

        write_header(
            &mut out,
            "aircd_wake_quota_cooldowns_active",
            "gauge",
            "Number of accounts currently paused due to provider quota cooldown.",
        );
        write_sample(
            &mut out,
            "aircd_wake_quota_cooldowns_active",
            &[],
            snap.wake_quota_cooldowns_active as u64,
        );

        write_header(
            &mut out,
            "aircd_wake_dispatch_duration_seconds",
            "histogram",
            "Latency in seconds from wake request acceptance to agent process spawn.",
        );
        write_histogram(
            &mut out,
            "aircd_wake_dispatch_duration_seconds",
            &[],
            &self.wake_dispatch_duration.snapshot(),
        );

        write_header(
            &mut out,
            "aircd_wake_duration_seconds",
            "histogram",
            "Execution wall-clock duration in seconds of launched wake runs by outcome.",
        );
        for outcome in WAKE_OUTCOMES {
            write_histogram(
                &mut out,
                "aircd_wake_duration_seconds",
                &[("outcome", outcome)],
                &self.wake_duration[outcome].snapshot(),
            );
        }

        write_header(
            &mut out,
            "aircd_wake_end_to_end_duration_seconds",
            "histogram",
            "Total wall-clock latency in seconds from wake request to run completion by outcome.",
        );
        for outcome in WAKE_OUTCOMES {
            write_histogram(
                &mut out,
                "aircd_wake_end_to_end_duration_seconds",
                &[("outcome", outcome)],
                &self.wake_end_to_end_duration[outcome].snapshot(),
            );
        }

        // 4. HTTP & Rate limiting
        write_header(
            &mut out,
            "aircd_http_requests_in_flight",
            "gauge",
            "Current number of in-flight HTTP API requests.",
        );
        write_sample_i64(
            &mut out,
            "aircd_http_requests_in_flight",
            &[],
            self.http_requests_in_flight.load(Ordering::Relaxed),
        );

        {
            let requests = self
                .http_requests_total
                .lock()
                .expect("metrics http requests mutex poisoned");
            write_header(
                &mut out,
                "aircd_http_requests_total",
                "counter",
                "Total HTTP requests served since server start by method and status class.",
            );
            for ((method, status_class), count) in requests.iter() {
                write_sample(
                    &mut out,
                    "aircd_http_requests_total",
                    &[("method", method), ("status_class", status_class)],
                    *count,
                );
            }
        }

        write_header(
            &mut out,
            "aircd_http_request_duration_seconds",
            "histogram",
            "HTTP request handling latency in seconds.",
        );
        write_histogram(
            &mut out,
            "aircd_http_request_duration_seconds",
            &[],
            &self.http_request_duration.snapshot(),
        );

        write_header(
            &mut out,
            "aircd_http_rate_limited_total",
            "counter",
            "Total HTTP requests rejected with 429 Too Many Requests by limit scope.",
        );
        for scope in RATE_LIMIT_SCOPES {
            let val = self.http_rate_limited_total[scope].load(Ordering::Relaxed);
            write_sample(
                &mut out,
                "aircd_http_rate_limited_total",
                &[("scope", scope)],
                val,
            );
        }

        // 5. Webhooks & SQLite pool
        write_header(
            &mut out,
            "aircd_webhook_deliveries_current",
            "gauge",
            "Current count of webhook deliveries in SQLite by status.",
        );
        for (status, count) in &snap.webhook_deliveries_by_status {
            write_sample(
                &mut out,
                "aircd_webhook_deliveries_current",
                &[("status", status)],
                *count,
            );
        }

        if let Some(pool) = &snap.db_pool {
            write_header(
                &mut out,
                "aircd_db_pool_readers_in_use",
                "gauge",
                "Number of SQLite read-pool connections currently checked out.",
            );
            write_sample(
                &mut out,
                "aircd_db_pool_readers_in_use",
                &[],
                pool.read_pool_in_use as u64,
            );

            write_header(
                &mut out,
                "aircd_db_pool_readers_idle",
                "gauge",
                "Number of SQLite read-pool connections currently idle.",
            );
            write_sample(
                &mut out,
                "aircd_db_pool_readers_idle",
                &[],
                pool.read_pool_idle as u64,
            );

            write_header(
                &mut out,
                "aircd_db_pool_readers_max",
                "gauge",
                "Configured maximum SQLite read-pool size.",
            );
            write_sample(
                &mut out,
                "aircd_db_pool_readers_max",
                &[],
                pool.read_pool_max as u64,
            );

            write_header(
                &mut out,
                "aircd_db_pool_acquires_total",
                "counter",
                "Total SQLite connection acquisitions by connection kind.",
            );
            write_sample(
                &mut out,
                "aircd_db_pool_acquires_total",
                &[("kind", "read")],
                pool.read_acquires,
            );
            write_sample(
                &mut out,
                "aircd_db_pool_acquires_total",
                &[("kind", "write")],
                pool.write_acquires,
            );

            write_header(
                &mut out,
                "aircd_db_pool_read_waits_total",
                "counter",
                "Total SQLite read-pool acquisitions that waited for a free reader.",
            );
            write_sample(&mut out, "aircd_db_pool_read_waits_total", &[], pool.read_waits);

            write_header(
                &mut out,
                "aircd_db_pool_wait_seconds_total",
                "counter",
                "Cumulative seconds spent waiting for SQLite connections by kind.",
            );
            write_sample_f64(
                &mut out,
                "aircd_db_pool_wait_seconds_total",
                &[("kind", "read")],
                pool.read_wait_ms as f64 / 1000.0,
            );
            write_sample_f64(
                &mut out,
                "aircd_db_pool_wait_seconds_total",
                &[("kind", "write")],
                pool.write_wait_ms as f64 / 1000.0,
            );

            write_header(
                &mut out,
                "aircd_db_wal_checkpoints_total",
                "counter",
                "Total SQLite WAL checkpoints performed.",
            );
            write_sample(
                &mut out,
                "aircd_db_wal_checkpoints_total",
                &[],
                pool.checkpoints,
            );

            write_header(
                &mut out,
                "aircd_db_vacuum_steps_total",
                "counter",
                "Total SQLite incremental vacuum steps executed.",
            );
            write_sample(
                &mut out,
                "aircd_db_vacuum_steps_total",
                &[],
                pool.vacuum_steps,
            );

            write_header(
                &mut out,
                "aircd_db_vacuum_pages_freed_total",
                "counter",
                "Total SQLite freelist pages reclaimed via incremental vacuum.",
            );
            write_sample(
                &mut out,
                "aircd_db_vacuum_pages_freed_total",
                &[],
                pool.vacuum_pages_freed,
            );

            write_header(
                &mut out,
                "aircd_db_vacuum_skipped_busy_total",
                "counter",
                "Total incremental vacuum attempts skipped because a writer held the lock.",
            );
            write_sample(
                &mut out,
                "aircd_db_vacuum_skipped_busy_total",
                &[],
                pool.vacuum_skipped_busy,
            );
        }

        if let Some(pages) = &snap.db_pages {
            write_header(
                &mut out,
                "aircd_db_page_size_bytes",
                "gauge",
                "Configured SQLite database page size in bytes.",
            );
            write_sample(&mut out, "aircd_db_page_size_bytes", &[], pages.page_size);

            write_header(
                &mut out,
                "aircd_db_pages_total",
                "gauge",
                "Total SQLite database pages.",
            );
            write_sample(&mut out, "aircd_db_pages_total", &[], pages.page_count);

            write_header(
                &mut out,
                "aircd_db_freelist_pages",
                "gauge",
                "Current SQLite freelist pages available for incremental vacuum.",
            );
            write_sample(&mut out, "aircd_db_freelist_pages", &[], pages.freelist_count);

            write_header(
                &mut out,
                "aircd_db_freelist_bytes",
                "gauge",
                "Current SQLite freelist size in bytes.",
            );
            write_sample(&mut out, "aircd_db_freelist_bytes", &[], pages.freelist_bytes);
        }

        // 6. Queue backlog, disk pressure & Prometheus alerting
        let webhook_pending = snap
            .webhook_deliveries_by_status
            .get("pending")
            .copied()
            .unwrap_or(0);
        write_header(
            &mut out,
            "aircd_queue_backlog_total",
            "gauge",
            "Total pending items across wake queue and webhook delivery queue.",
        );
        write_sample(
            &mut out,
            "aircd_queue_backlog_total",
            &[],
            snap.wake_pending_sessions as u64 + webhook_pending,
        );

        if let Some(disk) = &snap.disk {
            write_header(
                &mut out,
                "aircd_disk_total_bytes",
                "gauge",
                "Total filesystem capacity in bytes on the database volume.",
            );
            write_sample(&mut out, "aircd_disk_total_bytes", &[], disk.total_bytes);

            write_header(
                &mut out,
                "aircd_disk_available_bytes",
                "gauge",
                "Available filesystem bytes on the database volume.",
            );
            write_sample(
                &mut out,
                "aircd_disk_available_bytes",
                &[],
                disk.available_bytes,
            );

            write_header(
                &mut out,
                "aircd_disk_used_bytes",
                "gauge",
                "Used filesystem bytes on the database volume.",
            );
            write_sample(&mut out, "aircd_disk_used_bytes", &[], disk.used_bytes);

            write_header(
                &mut out,
                "aircd_disk_used_ratio",
                "gauge",
                "Filesystem used fraction (0.0..=1.0) on the database volume.",
            );
            write_sample_f64(&mut out, "aircd_disk_used_ratio", &[], disk.used_ratio);
        }

        if !snap.alert_states.is_empty() {
            write_header(
                &mut out,
                "aircd_alerts_active",
                "gauge",
                "Whether each configured Prometheus alert rule is currently firing (1) or not (0).",
            );
            for sample in &snap.alert_states {
                write_sample(
                    &mut out,
                    "aircd_alerts_active",
                    &[
                        ("rule", &sample.rule),
                        ("alertname", &sample.alertname),
                        ("severity", &sample.severity),
                    ],
                    u64::from(sample.active),
                );
            }

            write_header(
                &mut out,
                "aircd_alerts_state",
                "gauge",
                "Current alert rule state (0=inactive, 1=pending, 2=firing).",
            );
            for sample in &snap.alert_states {
                write_sample(
                    &mut out,
                    "aircd_alerts_state",
                    &[
                        ("rule", &sample.rule),
                        ("alertname", &sample.alertname),
                        ("severity", &sample.severity),
                    ],
                    u64::from(sample.state_code),
                );
            }
        }

        write_header(
            &mut out,
            "aircd_alerts_fired_total",
            "counter",
            "Total alert firing transitions since server start by rule.",
        );
        for rule in ALERT_RULE_KEYS {
            let val = self.alerts_fired_total[rule].load(Ordering::Relaxed);
            write_sample(&mut out, "aircd_alerts_fired_total", &[("rule", rule)], val);
        }

        write_header(
            &mut out,
            "aircd_alerts_resolved_total",
            "counter",
            "Total alert resolution transitions since server start by rule.",
        );
        for rule in ALERT_RULE_KEYS {
            let val = self.alerts_resolved_total[rule].load(Ordering::Relaxed);
            write_sample(
                &mut out,
                "aircd_alerts_resolved_total",
                &[("rule", rule)],
                val,
            );
        }

        write_header(
            &mut out,
            "aircd_alert_webhook_deliveries_total",
            "counter",
            "Total direct alert webhook deliveries attempted since server start by status.",
        );
        for status in ALERT_DELIVERY_STATUSES {
            let val = self.alert_webhook_deliveries_total[status].load(Ordering::Relaxed);
            write_sample(
                &mut out,
                "aircd_alert_webhook_deliveries_total",
                &[("status", status)],
                val,
            );
        }

        write_header(
            &mut out,
            "aircd_hub_draining",
            "gauge",
            "Whether the hub is currently draining for shutdown (1) or running normally (0).",
        );
        write_sample(&mut out, "aircd_hub_draining", &[], u64::from(snap.draining));

        write_header(
            &mut out,
            "aircd_shutdown_lease_checkpoints_total",
            "counter",
            "Total active task leases checkpointed during hub shutdown draining.",
        );
        write_sample(
            &mut out,
            "aircd_shutdown_lease_checkpoints_total",
            &[],
            self.shutdown_lease_checkpoints_total.load(Ordering::Relaxed),
        );

        write_header(
            &mut out,
            "aircd_shutdown_detached_workers_total",
            "counter",
            "Total worker sessions or wake executions cleanly detached during hub shutdown.",
        );
        write_sample(
            &mut out,
            "aircd_shutdown_detached_workers_total",
            &[],
            self.shutdown_detached_workers_total.load(Ordering::Relaxed),
        );

        write_header(
            &mut out,
            "aircd_shutdown_cancelled_wakes_total",
            "counter",
            "Total queued wakeups cancelled because hub wake dispatch was paused for shutdown.",
        );
        write_sample(
            &mut out,
            "aircd_shutdown_cancelled_wakes_total",
            &[],
            self.shutdown_cancelled_wakes_total.load(Ordering::Relaxed),
        );

        // 7. Audit log stream
        write_header(
            &mut out,
            "aircd_audit_stream_events_total",
            "counter",
            "Total structured audit events published to the audit stream since server start by category.",
        );
        for category in AUDIT_STREAM_CATEGORIES {
            let val = self.audit_stream_events_total[category].load(Ordering::Relaxed);
            write_sample(
                &mut out,
                "aircd_audit_stream_events_total",
                &[("category", category)],
                val,
            );
        }

        write_header(
            &mut out,
            "aircd_audit_stream_active_subscribers",
            "gauge",
            "Current number of active /v1/audit/stream HTTP subscribers.",
        );
        write_sample(
            &mut out,
            "aircd_audit_stream_active_subscribers",
            &[],
            self.audit_stream_active_subscribers
                .load(Ordering::Relaxed)
                .max(0) as u64,
        );

        write_header(
            &mut out,
            "aircd_audit_stream_lagged_total",
            "counter",
            "Total audit stream events dropped for slow subscribers due to broadcast lag.",
        );
        write_sample(
            &mut out,
            "aircd_audit_stream_lagged_total",
            &[],
            self.audit_stream_lagged_total.load(Ordering::Relaxed),
        );

        // 8. Dynamic TLS certificate hot-reload
        write_header(
            &mut out,
            "aircd_tls_reloads_total",
            "counter",
            "Total TLS certificate and mTLS CA hot-reload attempts by trigger and result.",
        );
        for &(trigger, result) in TLS_RELOAD_RESULTS {
            let val = self.tls_reloads_total[&(trigger, result)].load(Ordering::Relaxed);
            write_sample(
                &mut out,
                "aircd_tls_reloads_total",
                &[("trigger", trigger), ("result", result)],
                val,
            );
        }

        write_header(
            &mut out,
            "aircd_tls_reload_generation",
            "gauge",
            "Current active TLS certificate reload generation (0 when TLS is disabled).",
        );
        write_sample(
            &mut out,
            "aircd_tls_reload_generation",
            &[],
            self.tls_reload_generation.load(Ordering::Relaxed),
        );

        write_header(
            &mut out,
            "aircd_tls_last_reload_timestamp_seconds",
            "gauge",
            "Unix timestamp in seconds of the most recent TLS certificate load or hot-reload.",
        );
        write_sample(
            &mut out,
            "aircd_tls_last_reload_timestamp_seconds",
            &[],
            self.tls_last_reload_timestamp_seconds
                .load(Ordering::Relaxed),
        );

        out
    }
}

pub struct AuditStreamSubscriberGuard {
    registry: std::sync::Arc<MetricsRegistry>,
}

impl Drop for AuditStreamSubscriberGuard {
    fn drop(&mut self) {
        self.registry
            .audit_stream_active_subscribers
            .fetch_sub(1, Ordering::Relaxed);
    }
}

pub struct HttpInFlightGuard<'a> {
    registry: &'a MetricsRegistry,
}

impl Drop for HttpInFlightGuard<'_> {
    fn drop(&mut self) {
        self.registry
            .http_requests_in_flight
            .fetch_sub(1, Ordering::Relaxed);
    }
}

fn counter_map(keys: &[&'static str]) -> BTreeMap<&'static str, AtomicU64> {
    keys.iter().map(|&k| (k, AtomicU64::new(0))).collect()
}

fn histogram_map(
    keys: &[&'static str],
    bounds: &'static [f64],
) -> BTreeMap<&'static str, Histogram> {
    keys.iter()
        .map(|&k| (k, Histogram::new(bounds)))
        .collect()
}

fn write_header(out: &mut String, name: &str, kind: &str, help: &str) {
    out.push_str("# HELP ");
    out.push_str(name);
    out.push(' ');
    for ch in help.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('\n');
    out.push_str("# TYPE ");
    out.push_str(name);
    out.push(' ');
    out.push_str(kind);
    out.push('\n');
}

fn write_labels(out: &mut String, labels: &[(&str, &str)], extra: Option<(&str, &str)>) {
    if labels.is_empty() && extra.is_none() {
        return;
    }
    out.push('{');
    let mut first = true;
    for (k, v) in labels.iter().copied().chain(extra) {
        if !first {
            out.push(',');
        }
        first = false;
        out.push_str(k);
        out.push_str("=\"");
        for ch in v.chars() {
            match ch {
                '\\' => out.push_str("\\\\"),
                '"' => out.push_str("\\\""),
                '\n' => out.push_str("\\n"),
                c => out.push(c),
            }
        }
        out.push('"');
    }
    out.push('}');
}

fn write_sample(out: &mut String, name: &str, labels: &[(&str, &str)], value: u64) {
    out.push_str(name);
    write_labels(out, labels, None);
    let _ = writeln!(out, " {value}");
}

fn write_sample_i64(out: &mut String, name: &str, labels: &[(&str, &str)], value: i64) {
    out.push_str(name);
    write_labels(out, labels, None);
    let _ = writeln!(out, " {value}");
}

fn write_sample_f64(out: &mut String, name: &str, labels: &[(&str, &str)], value: f64) {
    out.push_str(name);
    write_labels(out, labels, None);
    out.push(' ');
    push_prom_f64(out, value);
    out.push('\n');
}

fn write_histogram(
    out: &mut String,
    name: &str,
    labels: &[(&str, &str)],
    snap: &HistogramSnapshot,
) {
    for (bound, cumulative) in &snap.buckets {
        let mut bound_str = String::new();
        push_prom_f64(&mut bound_str, *bound);
        out.push_str(name);
        out.push_str("_bucket");
        write_labels(out, labels, Some(("le", &bound_str)));
        let _ = writeln!(out, " {cumulative}");
    }
    out.push_str(name);
    out.push_str("_bucket");
    write_labels(out, labels, Some(("le", "+Inf")));
    let _ = writeln!(out, " {}", snap.count);

    out.push_str(name);
    out.push_str("_sum");
    write_labels(out, labels, None);
    out.push(' ');
    push_prom_f64(out, snap.sum);
    out.push('\n');

    out.push_str(name);
    out.push_str("_count");
    write_labels(out, labels, None);
    let _ = writeln!(out, " {}", snap.count);
}

fn push_prom_f64(out: &mut String, value: f64) {
    if value.is_nan() {
        out.push_str("NaN");
    } else if value == f64::INFINITY {
        out.push_str("+Inf");
    } else if value == f64::NEG_INFINITY {
        out.push_str("-Inf");
    } else {
        let _ = write!(out, "{value}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histogram_records_cumulative_buckets_sum_and_count() {
        let registry = MetricsRegistry::new();
        registry.record_wake_dispatch(0.0005); // <= 0.001
        registry.record_wake_dispatch(0.015); // <= 0.025
        registry.record_wake_dispatch(120.0); // > 60.0 (only +Inf)
        registry.record_wake_dispatch(f64::NAN); // ignored

        let rendered = registry.render_prometheus(&MetricsSnapshot::default());
        assert!(rendered.contains("aircd_wake_dispatch_duration_seconds_bucket{le=\"0.001\"} 1\n"));
        assert!(rendered.contains("aircd_wake_dispatch_duration_seconds_bucket{le=\"0.005\"} 1\n"));
        assert!(rendered.contains("aircd_wake_dispatch_duration_seconds_bucket{le=\"0.025\"} 2\n"));
        assert!(rendered.contains("aircd_wake_dispatch_duration_seconds_bucket{le=\"60\"} 2\n"));
        assert!(rendered.contains("aircd_wake_dispatch_duration_seconds_bucket{le=\"+Inf\"} 3\n"));
        assert!(rendered.contains("aircd_wake_dispatch_duration_seconds_count 3\n"));
        assert!(rendered.contains("aircd_wake_dispatch_duration_seconds_sum 120.0155\n"));
    }

    #[test]
    fn prometheus_escaping_and_line_endings_are_valid() {
        let registry = MetricsRegistry::new();
        registry.record_task_transition("in_progress", "testing\"quoted\\line\nbreak");
        let rendered = registry.render_prometheus(&MetricsSnapshot::default());
        assert!(!rendered.contains("\r\n"));
        assert!(rendered.contains(
            "aircd_task_transitions_total{from=\"in_progress\",to=\"testing\\\"quoted\\\\line\\nbreak\"} 1\n"
        ));
    }
}
