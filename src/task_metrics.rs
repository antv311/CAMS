//! Cycle-time, review-duration and time-in-status metrics derived from a
//! task's audit log (`task_events`), for `GET /v1/tasks/export`.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::tasks::{Task, TaskEvent, ALL_STATUSES};

/// Statuses that count as "work has started".
const STARTED_STATUSES: &[&str] = &["claimed", "in_progress", "testing"];
/// Statuses during which the task waits on reviewers.
const REVIEW_STATUSES: &[&str] = &["review", "super_review"];
/// Statuses that finish a task successfully.
const COMPLETED_STATUSES: &[&str] = &["merged", "done"];
/// Event actions that move a task between statuses. `review_submission`
/// duplicates its `status` event and `escalate*` record question states.
const TRANSITION_ACTIONS: &[&str] = &[
    "status",
    "assign",
    "unassign",
    "peer_review",
    "ask",
    "answer",
];

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct TaskMetrics {
    /// First move into claimed/in_progress/testing.
    pub started_at: Option<i64>,
    /// First hand-in for review.
    pub first_review_at: Option<i64>,
    /// Last move into merged/done (or `updated_at` for legacy tasks closed
    /// without an audit entry). `None` while open or when cancelled.
    pub completed_at: Option<i64>,
    /// created -> completed.
    pub lead_time_seconds: Option<i64>,
    /// started -> completed.
    pub cycle_time_seconds: Option<i64>,
    /// Total time spent in review/super_review (open intervals run to `now`).
    pub review_seconds: i64,
    /// Number of times the task entered review.
    pub review_rounds: usize,
    /// Number of times a reviewer sent it back.
    pub changes_requested: usize,
    /// Seconds spent in each status; the current status runs to `now` unless terminal.
    pub time_in_status: BTreeMap<String, i64>,
}

pub fn compute(task: &Task, events: &[TaskEvent], now: i64) -> TaskMetrics {
    let transitions: Vec<(&str, &str, i64)> = events
        .iter()
        .filter(|ev| TRANSITION_ACTIONS.contains(&ev.action.as_str()))
        .filter_map(|ev| {
            let to = ev.to_status.as_deref()?;
            let from = ev.from_status.as_deref().unwrap_or("");
            (from != to && ALL_STATUSES.contains(&to)).then_some((from, to, ev.created_at))
        })
        .collect();

    let mut metrics = TaskMetrics::default();
    let mut status = transitions
        .first()
        .map(|(from, _, _)| *from)
        .filter(|from| !from.is_empty())
        .unwrap_or("open")
        .to_string();
    let mut since = task.created_at;
    for &(_, to, at) in &transitions {
        let at = at.max(since);
        *metrics.time_in_status.entry(status.clone()).or_default() += at - since;
        if STARTED_STATUSES.contains(&to) && metrics.started_at.is_none() {
            metrics.started_at = Some(at);
        }
        if REVIEW_STATUSES.contains(&to) && !REVIEW_STATUSES.contains(&status.as_str()) {
            metrics.review_rounds += 1;
            metrics.first_review_at.get_or_insert(at);
        }
        if to == "changes_requested" {
            metrics.changes_requested += 1;
        }
        if COMPLETED_STATUSES.contains(&to) {
            metrics.completed_at = Some(at);
        } else if to != "cancelled" {
            // Reopened after completion.
            metrics.completed_at = None;
        }
        status = to.to_string();
        since = at;
    }
    if !crate::tasks::TERMINAL_STATUSES.contains(&status.as_str()) {
        *metrics.time_in_status.entry(status.clone()).or_default() += now.max(since) - since;
    }
    if status != task.status && COMPLETED_STATUSES.contains(&task.status.as_str()) {
        // Legacy claim/done flow does not write audit entries.
        metrics.completed_at = Some(task.updated_at);
    }
    if !COMPLETED_STATUSES.contains(&task.status.as_str()) {
        metrics.completed_at = None;
    }
    metrics.review_seconds = REVIEW_STATUSES
        .iter()
        .filter_map(|s| metrics.time_in_status.get(*s))
        .sum();
    metrics.lead_time_seconds = metrics.completed_at.map(|done| done - task.created_at);
    metrics.cycle_time_seconds = metrics
        .completed_at
        .zip(metrics.started_at)
        .map(|(done, start)| done - start);
    metrics
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Stats {
    pub count: usize,
    pub mean: Option<f64>,
    pub median: Option<f64>,
    pub min: Option<i64>,
    pub max: Option<i64>,
}

impl Stats {
    fn of(mut values: Vec<i64>) -> Self {
        values.sort_unstable();
        let count = values.len();
        if count == 0 {
            return Self::default();
        }
        let mid = count / 2;
        let median = if count % 2 == 0 {
            (values[mid - 1] + values[mid]) as f64 / 2.0
        } else {
            values[mid] as f64
        };
        Self {
            count,
            mean: Some(values.iter().sum::<i64>() as f64 / count as f64),
            median: Some(median),
            min: values.first().copied(),
            max: values.last().copied(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Summary {
    pub tasks: usize,
    pub completed: usize,
    pub by_status: BTreeMap<String, usize>,
    pub by_kind: BTreeMap<String, usize>,
    pub by_priority: BTreeMap<String, usize>,
    pub lead_time_seconds: Stats,
    pub cycle_time_seconds: Stats,
    /// Over tasks that reached review at least once.
    pub review_seconds: Stats,
    pub review_rounds: usize,
    pub changes_requested: usize,
}

pub fn summarize(rows: &[(&Task, &TaskMetrics)]) -> Summary {
    let mut summary = Summary {
        tasks: rows.len(),
        ..Summary::default()
    };
    for (task, metrics) in rows {
        *summary.by_status.entry(task.status.clone()).or_default() += 1;
        *summary.by_kind.entry(task.kind.clone()).or_default() += 1;
        *summary
            .by_priority
            .entry(task.priority.clone())
            .or_default() += 1;
        summary.completed += usize::from(metrics.completed_at.is_some());
        summary.review_rounds += metrics.review_rounds;
        summary.changes_requested += metrics.changes_requested;
    }
    let collect = |f: &dyn Fn(&TaskMetrics) -> Option<i64>| {
        Stats::of(rows.iter().filter_map(|(_, m)| f(m)).collect())
    };
    summary.lead_time_seconds = collect(&|m| m.lead_time_seconds);
    summary.cycle_time_seconds = collect(&|m| m.cycle_time_seconds);
    summary.review_seconds = collect(&|m| (m.review_rounds > 0).then_some(m.review_seconds));
    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(status: &str, created_at: i64, updated_at: i64) -> Task {
        Task {
            id: "t".into(),
            channel: "#one".into(),
            title: "T".into(),
            status: status.into(),
            claimed_by: None,
            lease_expires_at: None,
            body: None,
            kind: "task".into(),
            priority: "normal".into(),
            assignee: None,
            created_by: None,
            repo: None,
            branch: None,
            issue_url: None,
            created_at,
            updated_at,
            sprint: None,
            reviewer: None,
            parent_task_id: None,
            review_round: None,
            blocked_by: Vec::new(),
        }
    }

    fn ev(action: &str, from: &str, to: &str, at: i64) -> TaskEvent {
        TaskEvent {
            task_id: "t".into(),
            actor_id: "a".into(),
            action: action.into(),
            from_status: Some(from.into()),
            to_status: Some(to.into()),
            note: None,
            created_at: at,
        }
    }

    #[test]
    fn full_review_cycle_with_one_rejection() {
        let events = [
            ev("assign", "open", "assigned", 100),
            ev("status", "assigned", "in_progress", 150),
            ev("status", "in_progress", "review", 400),
            ev("review_submission", "in_progress", "review", 400),
            ev("status", "review", "changes_requested", 500),
            ev("status", "changes_requested", "in_progress", 520),
            ev("status", "in_progress", "review", 700),
            ev("peer_review", "review", "super_review", 760),
            ev("status", "super_review", "merged", 800),
        ];
        let m = compute(&task("merged", 0, 800), &events, 10_000);
        assert_eq!(m.started_at, Some(150));
        assert_eq!(m.first_review_at, Some(400));
        assert_eq!(m.completed_at, Some(800));
        assert_eq!(m.lead_time_seconds, Some(800));
        assert_eq!(m.cycle_time_seconds, Some(650));
        assert_eq!(m.review_rounds, 2);
        assert_eq!(m.changes_requested, 1);
        // 400..500 in review, 700..760 review + 760..800 super_review.
        assert_eq!(m.review_seconds, 100 + 60 + 40);
        assert_eq!(m.time_in_status["open"], 100);
        assert_eq!(m.time_in_status["in_progress"], 250 + 180);
        assert!(!m.time_in_status.contains_key("merged"));
    }

    #[test]
    fn open_task_counts_to_now_and_has_no_completion() {
        let events = [
            ev("assign", "open", "assigned", 10),
            ev("status", "assigned", "review", 20),
            ev("ask", "review", "blocked", 30),
            ev("escalate_pm", "open", "escalated_pm", 35),
        ];
        let m = compute(&task("blocked", 0, 30), &events, 100);
        assert_eq!(m.completed_at, None);
        assert_eq!(m.cycle_time_seconds, None);
        assert_eq!(m.review_seconds, 10);
        assert_eq!(m.time_in_status["blocked"], 70);
        assert_eq!(m.time_in_status.get("escalated_pm"), None);
    }

    #[test]
    fn legacy_done_without_events_uses_updated_at() {
        let m = compute(&task("done", 5, 65), &[], 1_000);
        assert_eq!(m.completed_at, Some(65));
        assert_eq!(m.lead_time_seconds, Some(60));
        assert_eq!(m.cycle_time_seconds, None);
    }

    #[test]
    fn summary_stats() {
        let a = task("merged", 0, 0);
        let mut b = task("cancelled", 0, 0);
        b.kind = "bug".into();
        let ma = TaskMetrics {
            lead_time_seconds: Some(10),
            cycle_time_seconds: Some(4),
            completed_at: Some(10),
            review_rounds: 1,
            review_seconds: 3,
            ..TaskMetrics::default()
        };
        let mb = TaskMetrics::default();
        let s = summarize(&[(&a, &ma), (&b, &mb), (&a, &ma)]);
        assert_eq!(s.tasks, 3);
        assert_eq!(s.completed, 2);
        assert_eq!(s.by_status["cancelled"], 1);
        assert_eq!(s.by_kind["bug"], 1);
        assert_eq!(s.by_priority["normal"], 3);
        assert_eq!(s.cycle_time_seconds.median, Some(4.0));
        assert_eq!(s.review_seconds.count, 2);
        assert_eq!(Stats::of(vec![1, 4, 2, 9]).median, Some(3.0));
        assert_eq!(Stats::of(vec![]).mean, None);
    }
}
