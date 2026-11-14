//! Lightweight sprint-monitoring web dashboard assets and snapshot builder.

use anyhow::Result;
use serde::Serialize;
use std::collections::HashMap;

use crate::auth::Principal;
use crate::gits::GitRepo;
use crate::history::unix_timestamp;
use crate::locks::ResourceLock;
use crate::projects::{Project, ProjectMember};
use crate::rooms::Room;
use crate::server::{ChannelSummary, Server};
use crate::tasks::{Task, TaskQuestion, TERMINAL_STATUSES};
use crate::wake::AgentWakeSummary;

pub const DASHBOARD_HTML: &str = include_str!("../web/dashboard/index.html");
pub const DASHBOARD_CSS: &str = include_str!("../web/dashboard/style.css");
pub const DASHBOARD_JS: &str = include_str!("../web/dashboard/app.js");

#[derive(Clone, Debug, Default, Serialize)]
pub struct StatusCounts {
    pub total: usize,
    pub open: usize,
    pub claimed: usize,
    pub assigned: usize,
    pub in_progress: usize,
    pub testing: usize,
    pub blocked: usize,
    pub review: usize,
    pub super_review: usize,
    pub changes_requested: usize,
    pub merged: usize,
    pub done: usize,
    pub cancelled: usize,
    pub dormant: usize,
    pub skipped: usize,
    pub completed: usize,
    pub active: usize,
}

impl StatusCounts {
    fn record(&mut self, status: &str) {
        self.total += 1;
        match status {
            "open" => self.open += 1,
            "claimed" => self.claimed += 1,
            "assigned" => self.assigned += 1,
            "in_progress" => self.in_progress += 1,
            "testing" => self.testing += 1,
            "blocked" => self.blocked += 1,
            "review" => self.review += 1,
            "super_review" => self.super_review += 1,
            "changes_requested" => self.changes_requested += 1,
            "merged" => self.merged += 1,
            "done" => self.done += 1,
            "cancelled" => self.cancelled += 1,
            "dormant" => self.dormant += 1,
            "skipped" => self.skipped += 1,
            _ => {}
        }
        if matches!(status, "merged" | "done") {
            self.completed += 1;
        }
        if !TERMINAL_STATUSES.contains(&status) {
            self.active += 1;
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct DashboardViewer {
    pub id: String,
    pub nick: String,
    pub role: String,
    pub team: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct DashboardSummary {
    pub projects_total: usize,
    pub rooms_total: usize,
    pub filtered_task_counts: StatusCounts,
    pub pending_questions_total: usize,
    pub agents_total: usize,
    pub agents_online: usize,
    pub irc_sessions: usize,
    pub wake_running_sessions: usize,
    pub wake_pending_sessions: usize,
    pub wake_stored_sessions: usize,
    pub wake_quota_cooldowns_active: usize,
    pub messages_total: u64,
    pub locks_total: usize,
    pub git_repos_total: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct DashboardProject {
    #[serde(flatten)]
    pub project: Project,
    pub members: Vec<ProjectMember>,
    pub task_counts: StatusCounts,
}

#[derive(Clone, Debug, Serialize)]
pub struct DashboardRoom {
    #[serde(flatten)]
    pub room: Room,
    pub task_counts: StatusCounts,
}

#[derive(Clone, Debug, Serialize)]
pub struct DashboardAgent {
    pub id: String,
    pub nick: String,
    pub team: Option<String>,
    pub role: String,
    pub online: bool,
    pub webhook_configured: bool,
    pub active_task_count: usize,
    pub wake: AgentWakeSummary,
}

#[derive(Clone, Debug, Serialize)]
pub struct DashboardSnapshot {
    pub ok: bool,
    pub generated_at: i64,
    pub viewer: DashboardViewer,
    pub summary: DashboardSummary,
    pub projects: Vec<DashboardProject>,
    pub rooms: Vec<DashboardRoom>,
    pub tasks: Vec<Task>,
    pub pending_questions: Vec<TaskQuestion>,
    pub agents: Vec<DashboardAgent>,
    pub git_repos: Vec<GitRepo>,
    pub locks: Vec<ResourceLock>,
    pub channels: Vec<ChannelSummary>,
}

pub fn build_dashboard_snapshot(
    server: &Server,
    viewer: &Principal,
    channel: Option<&str>,
    sprint: Option<&str>,
    include_terminal: bool,
) -> Result<DashboardSnapshot> {
    let dir = server.directory_snapshot()?;
    let metrics = server.metrics_snapshot()?;
    let projects_raw = server.projects().list()?;
    let rooms_raw = server.rooms().list(None)?;
    let all_tasks = server.tasks().list_all(None, None, true)?;
    let pending_questions = server.tasks().pending_questions_all(channel)?;
    let wake_summaries = server.wakes().agent_wake_summaries()?;

    let mut project_counts: HashMap<String, StatusCounts> = HashMap::new();
    let mut room_counts: HashMap<String, StatusCounts> = HashMap::new();
    let mut active_by_agent: HashMap<String, usize> = HashMap::new();
    let mut filtered_task_counts = StatusCounts::default();

    for task in &all_tasks {
        project_counts
            .entry(task.channel.clone())
            .or_default()
            .record(&task.status);
        if let Some(sp) = task.sprint.as_deref() {
            room_counts
                .entry(sp.to_string())
                .or_default()
                .record(&task.status);
        }
        if !TERMINAL_STATUSES.contains(&task.status.as_str()) {
            let owner = task
                .assignee
                .as_deref()
                .or_else(|| (task.status == "claimed").then_some(task.claimed_by.as_deref()).flatten());
            if let Some(id) = owner {
                *active_by_agent.entry(id.to_ascii_lowercase()).or_insert(0) += 1;
            }
        }
        let channel_ok = channel.is_none_or(|c| task.channel == c);
        let sprint_ok = sprint.is_none_or(|s| task.sprint.as_deref() == Some(s));
        if channel_ok && sprint_ok {
            filtered_task_counts.record(&task.status);
        }
    }

    let tasks: Vec<Task> = all_tasks
        .into_iter()
        .filter(|task| channel.is_none_or(|c| task.channel == c))
        .filter(|task| sprint.is_none_or(|s| task.sprint.as_deref() == Some(s)))
        .filter(|task| include_terminal || !TERMINAL_STATUSES.contains(&task.status.as_str()))
        .collect();

    let mut projects = Vec::with_capacity(projects_raw.len());
    for project in projects_raw {
        let members = server.projects().members(&project.channel)?;
        let task_counts = project_counts.remove(&project.channel).unwrap_or_default();
        projects.push(DashboardProject {
            project,
            members,
            task_counts,
        });
    }

    let rooms: Vec<DashboardRoom> = rooms_raw
        .into_iter()
        .map(|room| {
            let task_counts = room_counts.remove(&room.channel).unwrap_or_default();
            DashboardRoom { room, task_counts }
        })
        .collect();

    let agents_online = dir.agents.iter().filter(|a| a.online).count();
    let agents: Vec<DashboardAgent> = dir
        .agents
        .into_iter()
        .map(|a| {
            let key = a.id.to_ascii_lowercase();
            let wake = wake_summaries.get(&key).cloned().unwrap_or_default();
            let active_task_count = active_by_agent.get(&key).copied().unwrap_or(0);
            DashboardAgent {
                id: a.id,
                nick: a.nick,
                team: a.team,
                role: a.role,
                online: a.online,
                webhook_configured: a.webhook_configured,
                active_task_count,
                wake,
            }
        })
        .collect();

    let summary = DashboardSummary {
        projects_total: projects.len(),
        rooms_total: rooms.len(),
        filtered_task_counts,
        pending_questions_total: pending_questions.len(),
        agents_total: agents.len(),
        agents_online,
        irc_sessions: metrics.irc_sessions,
        wake_running_sessions: metrics.wake_running_sessions,
        wake_pending_sessions: metrics.wake_pending_sessions,
        wake_stored_sessions: metrics.wake_stored_sessions,
        wake_quota_cooldowns_active: metrics.wake_quota_cooldowns_active,
        messages_total: metrics.messages_total,
        locks_total: dir.locks.len(),
        git_repos_total: dir.git_repos.len(),
    };

    Ok(DashboardSnapshot {
        ok: true,
        generated_at: unix_timestamp(),
        viewer: DashboardViewer {
            id: viewer.id.clone(),
            nick: viewer.nick.clone(),
            role: viewer.role.clone(),
            team: viewer.team.clone(),
        },
        summary,
        projects,
        rooms,
        tasks,
        pending_questions,
        agents,
        git_repos: dir.git_repos,
        locks: dir.locks,
        channels: dir.channels,
    })
}
