//! `aircd tui`: a terminal dashboard, chat and control panel for the hub.
//!
//! The server keeps running on its own; this mode attaches to it over the
//! local HTTP API using a saved `airc` profile. Closing the interface never
//! stops the hub or the agents it has launched.

use anyhow::{anyhow, Context, Result};
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use ratatui::Frame;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

const REFRESH_EVERY: Duration = Duration::from_secs(2);
const NOTICE_FOR: Duration = Duration::from_secs(6);
const TABS: &str = " [H]ome  [D]ashboard  [C]hat  [Q]uit ";
const CHANNEL_PANE_WIDTH: u16 = 26;
const PEOPLE_PANE_WIDTH: u16 = 26;
const STATUS_CHOICES: &[&str] = &[
    "in_progress",
    "testing",
    "review",
    "changes_requested",
    "merged",
    "cancelled",
];
const ROLE_CHOICES: &[&str] = &["member", "builder", "reviewer", "tester", "coordinator"];
const KIND_CHOICES: &[&str] = &["task", "bug", "feature"];
/// Picker values that start a step instead of naming an account or model.
/// Account names cannot start with `+`.
const NEW_ACCOUNT: &str = "+new";
const TYPE_MODEL: &str = "+type";
const HELP: &[&str] = &[
    "Home",
    "  up/down     choose a project      Enter  open its dashboard",
    "  n           new project (name, then its repo folder)",
    "  m / p       add a member / choose the project manager",
    "  t           send a test wake to an agent (uses its quota)",
    "",
    "Dashboard  (Esc returns to Home)",
    "  up/down     move between projects and tasks",
    "  Enter       show the selected task with its history",
    "  n           new task in the selected project",
    "  o           open the project's next sprint room",
    "  a           assign or reassign the selected task",
    "  s           move the selected task to another status",
    "  p           choose the selected project's manager (owner)",
    "  m           add an account to the selected project, or create one",
    "              for a client and model (owner)",
    "  w           answer a question that is waiting on you",
    "  l           switch an account's launcher on or off (owner)",
    "  c           chat      r  refresh      q  quit",
    "",
    "Chat",
    "  Enter send · Tab complete @name · up/down channel · Esc dashboard",
    "  PageUp/PageDown scroll history · Home oldest · End latest",
    "  Mouse wheel scrolls history over the message pane",
    "  Ctrl+D or click a name in People: direct message someone",
    "",
    "In a pop-up: up/down choose, Enter confirm, Esc cancel.",
];

// ---------------------------------------------------------------- data

#[derive(Clone, Debug, Default)]
pub struct Account {
    /// Principal id: DM history names conversations by id.
    pub id: String,
    pub nick: String,
    pub role: String,
    pub online: bool,
    pub running: bool,
    /// `None`: no launcher. `Some(enabled)` otherwise.
    pub launcher: Option<bool>,
    /// The launcher's executable exists on the hub's machine.
    pub command_found: bool,
    /// Outcome of the most recent launch: ok, failed, out of quota, ...
    pub last_run: Option<String>,
}

/// A repo attached to a project, with what could be checked locally.
#[derive(Clone, Debug, Default)]
pub struct RepoView {
    pub name: String,
    pub folder_found: bool,
    pub local_ok: bool,
    pub branch: Option<String>,
    pub remote: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct TaskView {
    pub id: String,
    pub kind: String,
    pub status: String,
    pub assignee: String,
    pub title: String,
    /// The sprint room the task belongs to.
    pub sprint: Option<String>,
    /// Account that reviews the work: the parent's lead or the project PM.
    pub reviewer: Option<String>,
    /// The task this one is a subtask of.
    pub parent: Option<String>,
}

/// A sprint room under a project.
#[derive(Clone, Debug, Default)]
pub struct RoomView {
    pub channel: String,
    pub label: String,
    pub title: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Member {
    pub nick: String,
    pub role: String,
}

#[derive(Clone, Debug, Default)]
pub struct ProjectView {
    pub channel: String,
    pub name: String,
    pub pm: Option<String>,
    pub tasks: Vec<TaskView>,
    pub members: Vec<Member>,
    pub repos: Vec<RepoView>,
    pub rooms: Vec<RoomView>,
}

/// A question that is stopping a task until this user answers it.
#[derive(Clone, Debug, Default)]
pub struct Waiting {
    pub task_id: String,
    pub channel: String,
    pub asked_by: String,
    pub question: String,
    pub escalated: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Launcher {
    pub nick: String,
    pub enabled: bool,
    pub mode: String,
}

#[derive(Clone, Debug, Default)]
pub struct ChatLine {
    pub seq: i64,
    pub sender: String,
    pub body: String,
    pub time: i64,
}

/// A client template an account can be created from.
#[derive(Clone, Debug, Default)]
pub struct TemplateView {
    pub id: String,
    pub name: String,
    pub installed: bool,
}

/// A model a client can run: its id and, when the client gave one, a label.
pub type ModelItem = (String, Option<String>);

/// Everything the screens show, refreshed from the server in the background.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub server: String,
    pub me: String,
    pub my_role: String,
    pub accounts: Vec<Account>,
    pub projects: Vec<ProjectView>,
    pub waiting: Vec<Waiting>,
    pub launchers: Vec<Launcher>,
    pub runs: Vec<String>,
    pub channels: Vec<String>,
    /// Room history by channel; direct messages are keyed `@nick`.
    pub messages: BTreeMap<String, Vec<ChatLine>>,
    /// Highest direct-message seq fetched so far.
    pub dm_seq: i64,
    /// The task opened with Enter: its id and the lines to show.
    pub detail: Option<(String, Vec<String>)>,
    pub templates: Vec<TemplateView>,
    /// The models last listed for a template, or why they could not be.
    pub models: Option<(String, Result<Vec<ModelItem>, String>)>,
    /// The result of the last action: (succeeded, text, when).
    pub notice: Option<(bool, String, Instant)>,
    /// The hub could not be reached or refused the refresh.
    pub error: Option<String>,
}

#[derive(Clone, Debug)]
struct TuiConfig {
    server: String,
    token: String,
    nick: String,
    owner_key: Option<String>,
}

#[derive(Debug, PartialEq)]
pub enum Action {
    Quit,
    Refresh,
    Send { target: String, body: String },
    /// POST `body` to `path`. `owner` adds the owner key from this machine.
    Api { path: String, body: Value, owner: bool, done: String },
    LoadTask(String),
    /// Ask the hub which models a client template offers.
    LoadModels(String),
    /// Create an account from a template (owner) and save its airc profile.
    CreateAccount { body: Value },
    /// Several requests in order; stops at the first that fails.
    Many(Vec<Action>),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum View {
    Home,
    Dashboard,
    Chat,
}

/// Where a pop-up's answer goes next.
#[derive(Clone, Debug, PartialEq)]
pub enum Flow {
    NewTitle { channel: String },
    NewAssignee { channel: String, title: String },
    NewKind { channel: String, title: String, assignee: String },
    NewBody { channel: String, title: String, assignee: String, kind: String },
    Assign { task_id: String },
    Status { task_id: String },
    StatusNote { task_id: String, status: String },
    SetPm { channel: String },
    AddMember { channel: String },
    AddMemberRole { channel: String, nick: String },
    AnswerPick,
    AnswerText { task_id: String },
    ToggleLauncher,
    NewProjectName,
    NewProjectRepo { name: String },
    TestAgent,
    OpenSprint { channel: String },
    NewAccountClient { channel: String },
    NewAccountModel { channel: String, template: String },
    NewAccountName { channel: String, template: String, model: String },
    NewAccountRole { channel: String, template: String, model: String, nick: String },
    OpenDm,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Modal {
    /// Choose one of `items` (label, value).
    Picker { title: String, items: Vec<(String, String)>, idx: usize, flow: Flow },
    /// Type a line of text.
    Prompt { title: String, text: String, flow: Flow },
    /// Choose a model of `template`; the list arrives from the hub.
    Models { channel: String, template: String, idx: usize },
    Detail { task_id: String },
    Help,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Row {
    Project(usize),
    Task(usize, usize),
}

pub struct App {
    pub view: View,
    pub channel_idx: usize,
    pub input: String,
    /// Selected row in the projects-and-tasks list.
    pub cursor: usize,
    pub modal: Option<Modal>,
    /// The project opened from Home; the dashboard shows only its tasks.
    pub project: Option<String>,
    pub home_cursor: usize,
    /// Last message seen in each room, for the unread counts.
    pub seen: HashMap<String, i64>,
    /// Rooms first shown later count messages after this seq as unread.
    pub seen_floor: Option<i64>,
    /// Direct-message conversations opened here that have no history yet.
    pub dm_open: Vec<String>,
    /// Wrapped rows above the latest messages; zero follows new messages.
    pub chat_scroll: usize,
    chat_max_scroll: usize,
    chat_content_height: usize,
    chat_area: Rect,
    chat_channel: Option<String>,
    channel_rows: Vec<(u16, usize)>,
    task_rows: Vec<(u16, usize)>,
    home_rows: Vec<(u16, usize)>,
    /// People pane rows that open a direct message: (y, nick).
    people_rows: Vec<(u16, String)>,
    people_area: Rect,
    scroll: usize,
}

impl Default for App {
    fn default() -> Self {
        Self {
            view: View::Dashboard,
            channel_idx: 0,
            input: String::new(),
            cursor: 0,
            modal: None,
            project: None,
            home_cursor: 0,
            seen: HashMap::new(),
            seen_floor: None,
            dm_open: Vec::new(),
            chat_scroll: 0,
            chat_max_scroll: 0,
            chat_content_height: 0,
            chat_area: Rect::default(),
            chat_channel: None,
            channel_rows: Vec::new(),
            task_rows: Vec::new(),
            home_rows: Vec::new(),
            people_rows: Vec::new(),
            people_area: Rect::default(),
            scroll: 0,
        }
    }
}

/// The rooms the chat lists: channels, then direct messages by name.
pub fn chat_rooms(app: &App, snap: &Snapshot) -> Vec<String> {
    let (mut rooms, mut dms): (Vec<String>, Vec<String>) = snap
        .channels
        .iter()
        .cloned()
        .partition(|room| !room.starts_with('@'));
    dms.extend(app.dm_open.iter().cloned());
    dms.sort_by_key(|dm| dm.to_ascii_lowercase());
    dms.dedup();
    rooms.extend(dms);
    rooms
}

/// Open the direct-message conversation with `nick` in the chat.
pub fn open_dm(app: &mut App, nick: &str) {
    let room = format!("@{}", nick.trim_start_matches('@'));
    if !app.dm_open.contains(&room) {
        app.dm_open.push(room.clone());
    }
    // Drawing moves the selection to this room once it is listed.
    app.chat_channel = Some(room);
    app.chat_scroll = 0;
    app.input.clear();
    app.view = View::Chat;
}

/// Messages from others in `room` after the last one seen there.
pub fn unread(app: &App, snap: &Snapshot, room: &str) -> usize {
    let seen = app
        .seen
        .get(room)
        .copied()
        .or(app.seen_floor)
        .unwrap_or(i64::MAX);
    snap.messages
        .get(room)
        .map(|lines| {
            lines
                .iter()
                .filter(|line| line.seq > seen && !line.sender.eq_ignore_ascii_case(&snap.me))
                .count()
        })
        .unwrap_or(0)
}

/// The project a room belongs to: its own channel or one of its sprint rooms.
fn project_for<'a>(snap: &'a Snapshot, channel: &str) -> Option<&'a ProjectView> {
    snap.projects
        .iter()
        .find(|p| p.channel == channel || p.rooms.iter().any(|r| r.channel == channel))
}

/// The projects-and-tasks list, flattened in display order.
pub fn rows(snap: &Snapshot, only: Option<&str>) -> Vec<Row> {
    let mut out = Vec::new();
    for (p, project) in snap.projects.iter().enumerate() {
        if only.is_some_and(|channel| channel != project.channel) {
            continue;
        }
        out.push(Row::Project(p));
        out.extend(task_tree(project).into_iter().map(|(t, _)| Row::Task(p, t)));
    }
    out
}

/// A project's tasks in tree order: each task is followed by its subtasks.
/// Returns `(task index, depth)`. Subtasks whose parent is not in the list
/// show at the top level, and a parent cycle cannot loop forever.
pub fn task_tree(project: &ProjectView) -> Vec<(usize, usize)> {
    let tasks = &project.tasks;
    let index_of = |id: &str| tasks.iter().position(|task| task.id == id);
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); tasks.len()];
    let mut roots = Vec::new();
    for (t, task) in tasks.iter().enumerate() {
        match task.parent.as_deref().and_then(index_of) {
            Some(parent) if parent != t => children[parent].push(t),
            _ => roots.push(t),
        }
    }
    let mut out = Vec::with_capacity(tasks.len());
    let mut seen = vec![false; tasks.len()];
    // Tasks caught in a parent cycle have no root; list them at the top level.
    for start in roots.into_iter().chain(0..tasks.len()) {
        let mut stack = vec![(start, 0)];
        while let Some((t, depth)) = stack.pop() {
            if seen[t] {
                continue;
            }
            seen[t] = true;
            out.push((t, depth));
            stack.extend(children[t].iter().rev().map(|&c| (c, depth + 1)));
        }
    }
    out
}

fn selected_project<'a>(app: &App, snap: &'a Snapshot) -> Option<&'a ProjectView> {
    if app.view == View::Home {
        return snap.projects.get(app.home_cursor);
    }
    match rows(snap, app.project.as_deref()).get(app.cursor)? {
        Row::Project(p) | Row::Task(p, _) => snap.projects.get(*p),
    }
}

fn selected_task<'a>(app: &App, snap: &'a Snapshot) -> Option<&'a TaskView> {
    if app.view == View::Home {
        return None;
    }
    match rows(snap, app.project.as_deref()).get(app.cursor)? {
        Row::Task(p, t) => snap.projects.get(*p)?.tasks.get(*t),
        Row::Project(_) => None,
    }
}

// ---------------------------------------------------------------- config

fn config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("AIRCD_CONFIG_DIR") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir);
        }
    }
    if cfg!(windows) {
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            if !local.trim().is_empty() {
                return PathBuf::from(local).join("aircd");
            }
        }
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".config").join("aircd")
}

fn load_config(server: Option<String>, profile: Option<String>) -> Result<TuiConfig> {
    let dir = config_dir();
    let path = dir.join("profiles.json");
    let raw = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "no saved airc profile at {}: run `airc register` or `airc login` first",
            path.display()
        )
    })?;
    let data: Value = serde_json::from_str(&raw).context("parse profiles.json")?;
    let name = profile
        .or_else(|| data["default"].as_str().map(str::to_owned))
        .ok_or_else(|| anyhow!("no default airc profile; pass --profile <name>"))?;
    let entry = &data["profiles"][name.as_str()];
    let token = entry["token"]
        .as_str()
        .ok_or_else(|| anyhow!("airc profile '{name}' has no token"))?
        .to_string();
    let owner_key = std::fs::read_to_string(dir.join("aircd-owner.key"))
        .ok()
        .map(|key| key.trim().to_string())
        .filter(|key| !key.is_empty());
    Ok(TuiConfig {
        server: server
            .or_else(|| entry["server"].as_str().map(str::to_owned))
            .unwrap_or_else(|| "http://127.0.0.1:6680".to_string())
            .trim_end_matches('/')
            .to_string(),
        token,
        nick: entry["nick"].as_str().unwrap_or(&name).to_string(),
        owner_key,
    })
}

// ---------------------------------------------------------------- fetching

fn text(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_string()
}

fn encode_channel(channel: &str) -> String {
    channel.replace('#', "%23")
}

async fn request(
    client: &reqwest::Client,
    cfg: &TuiConfig,
    path: &str,
    body: Option<&Value>,
) -> Result<Value> {
    let url = format!("{}{path}", cfg.server);
    let builder = match body {
        Some(body) => client.post(url).json(body),
        None => client.get(url),
    };
    let response = builder
        .bearer_auth(&cfg.token)
        .send()
        .await
        .with_context(|| format!("cannot reach the hub at {}", cfg.server))?;
    let status = response.status();
    let parsed: Value = response.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        return Err(anyhow!(
            "{}",
            parsed["error"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| format!("{path}: HTTP {}", status.as_u16()))
        ));
    }
    Ok(parsed)
}

async fn get(client: &reqwest::Client, cfg: &TuiConfig, path: &str) -> Result<Value> {
    request(client, cfg, path, None).await
}

fn waiting_from(question: &Value, escalated: bool) -> Waiting {
    Waiting {
        task_id: text(&question["task_id"]),
        channel: text(&question["channel"]),
        asked_by: text(&question["asked_by"]),
        question: text(&question["question"]),
        escalated,
    }
}

/// Check an attached repo on this machine: no network, no git process.
fn repo_view(repo: &Value) -> RepoView {
    let path = text(&repo["local_path"]);
    let root = std::path::Path::new(&path);
    let git = root.join(".git");
    RepoView {
        name: text(&repo["name"]),
        folder_found: root.is_dir(),
        local_ok: root.is_dir() && git.exists(),
        branch: std::fs::read_to_string(git.join("HEAD")).ok().and_then(|head| {
            head.trim()
                .strip_prefix("ref: refs/heads/")
                .map(str::to_owned)
        }),
        remote: repo["remote_url"]
            .as_str()
            .filter(|url| !url.is_empty())
            .map(str::to_owned),
    }
}

async fn refresh(
    client: &reqwest::Client,
    cfg: &TuiConfig,
    shared: &Arc<Mutex<Snapshot>>,
) -> Result<()> {
    let mut snap = shared.lock().expect("snapshot mutex poisoned").clone();
    snap.server = cfg.server.clone();
    snap.me = cfg.nick.clone();

    // Owner-only views: who is running right now, launches and launchers.
    let mut running: HashSet<String> = HashSet::new();
    let mut last_run: HashMap<String, String> = HashMap::new();
    let mut launcher_state: HashMap<String, (bool, bool)> = HashMap::new();
    snap.runs.clear();
    snap.launchers.clear();
    if let Some(key) = cfg.owner_key.as_deref() {
        let wake = |action: &str| json!({"invite": key, "action": action, "limit": 40});
        if let Ok(body) = request(client, cfg, "/v1/owner/wake", Some(&wake("runs"))).await {
            for run in body["runs"].as_array().into_iter().flatten() {
                let status = text(&run["status"]);
                let who = text(&run["principal_id"]);
                if status == "running" {
                    running.insert(who.clone());
                }
                if snap.runs.len() < 12 {
                    snap.runs
                        .push(format!("{status:8} @{who}  {}", text(&run["reason"])));
                }
                // Runs come newest first: keep each account's latest outcome.
                let output = text(&run["log_tail"]).to_ascii_lowercase();
                let outcome = if status == "failed"
                    && (output.contains("quota") || output.contains("resource_exhausted"))
                {
                    "out of quota".to_string()
                } else {
                    status
                };
                last_run.entry(who).or_insert(outcome);
            }
        }
        if let Ok(body) = request(client, cfg, "/v1/owner/wake", Some(&wake("list"))).await {
            for launcher in body["launchers"].as_array().into_iter().flatten() {
                let nick = text(&launcher["principal_id"]);
                let enabled = launcher["enabled"].as_bool().unwrap_or(false);
                launcher_state.insert(
                    nick.clone(),
                    (enabled, launcher["command_found"].as_bool().unwrap_or(true)),
                );
                snap.launchers.push(Launcher {
                    nick,
                    enabled,
                    mode: text(&launcher["session_mode"]),
                });
            }
        }
    }

    let agents = get(client, cfg, "/v1/agents").await?;
    snap.accounts = agents["agents"]
        .as_array()
        .into_iter()
        .flatten()
        // The bundled demo and test accounts are noise on a real hub.
        .filter(|a| !matches!(a["team"].as_str(), Some("demo") | Some("test")))
        .map(|a| {
            let id = text(&a["id"]);
            let launcher = launcher_state.get(&id);
            Account {
                id: id.clone(),
                nick: text(&a["nick"]),
                role: text(&a["role"]),
                online: a["online"].as_bool().unwrap_or(false),
                running: running.contains(&id),
                launcher: launcher.map(|(enabled, _)| *enabled),
                command_found: launcher.is_some_and(|(_, found)| *found),
                last_run: last_run.get(&id).cloned(),
            }
        })
        .collect();
    if let Some(me) = snap
        .accounts
        .iter()
        .find(|a| a.nick.eq_ignore_ascii_case(&cfg.nick))
    {
        snap.my_role = me.role.clone();
    }

    // Older hubs have no templates; that only hides account creation.
    snap.templates = match get(client, cfg, "/v1/templates").await {
        Ok(body) => body["templates"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|t| TemplateView {
                id: text(&t["id"]),
                name: text(&t["name"]),
                installed: t["installed"].as_bool().unwrap_or(false),
            })
            .collect(),
        Err(_) => Vec::new(),
    };

    let projects = get(client, cfg, "/v1/projects").await?;
    let gits = get(client, cfg, "/v1/gits").await?;
    let rooms = get(client, cfg, "/v1/rooms").await?;
    let mut views = Vec::new();
    for project in projects["projects"].as_array().into_iter().flatten() {
        let channel = text(&project["channel"]);
        let encoded = encode_channel(&channel);
        let tasks = get(client, cfg, &format!("/v1/tasks?channel={encoded}")).await?;
        let members = get(client, cfg, &format!("/v1/projects/members?channel={encoded}")).await?;
        views.push(ProjectView {
            name: text(&project["name"]),
            pm: project["pm"].as_str().map(str::to_owned),
            tasks: tasks["tasks"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|t| TaskView {
                    id: text(&t["id"]),
                    kind: text(&t["kind"]),
                    status: text(&t["status"]),
                    assignee: t["assignee"]
                        .as_str()
                        .or(t["claimed_by"].as_str())
                        .unwrap_or("-")
                        .to_string(),
                    title: text(&t["title"]),
                    sprint: t["sprint"].as_str().map(str::to_owned),
                    reviewer: t["reviewer"].as_str().map(str::to_owned),
                    parent: t["parent_task_id"].as_str().map(str::to_owned),
                })
                .collect(),
            members: members["members"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|m| Member {
                    nick: text(&m["principal_id"]),
                    role: text(&m["role"]),
                })
                .collect(),
            repos: gits["git_repos"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|repo| repo["channel"].as_str() == Some(channel.as_str()))
                .map(repo_view)
                .collect(),
            rooms: rooms["rooms"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|room| room["project"].as_str() == Some(channel.as_str()))
                .map(|room| RoomView {
                    channel: text(&room["channel"]),
                    label: text(&room["label"]),
                    title: room["title"].as_str().map(str::to_owned),
                })
                .collect(),
            channel,
        });
    }
    snap.projects = views;

    let inbox = get(client, cfg, "/v1/inbox").await?;
    let inbox = &inbox["inbox"];
    let mut waiting: Vec<Waiting> = inbox["needs_you"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|q| waiting_from(q, true))
        .collect();
    for project in inbox["projects"].as_array().into_iter().flatten() {
        for question in project["questions"].as_array().into_iter().flatten() {
            waiting.push(waiting_from(question, false));
        }
    }
    snap.waiting = waiting;

    // Channels: #general first, then every project and any other channel.
    let listed = get(client, cfg, "/v1/channels").await?;
    // Order: #general and other hub rooms first, then each project's rooms.
    let project_channels: Vec<String> = snap
        .projects
        .iter()
        .flat_map(|p| {
            std::iter::once(p.channel.clone()).chain(p.rooms.iter().map(|r| r.channel.clone()))
        })
        .collect();
    let mut channels = vec!["#general".to_string()];
    for name in listed["channels"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| text(&c["channel"]))
        .filter(|name| !project_channels.contains(name))
        .chain(project_channels.iter().cloned())
    {
        if name.starts_with('#') && !channels.contains(&name) {
            channels.push(name);
        }
    }
    snap.channels = channels;

    for channel in snap.channels.clone() {
        let lines = snap.messages.entry(channel.clone()).or_default();
        let after = lines.last().map(|line| line.seq).unwrap_or(0);
        let fetched = get(
            client,
            cfg,
            &format!(
                "/v1/messages?channel={}&after_seq={after}&limit=500",
                encode_channel(&channel)
            ),
        )
        .await?;
        for message in fetched["messages"].as_array().into_iter().flatten() {
            lines.push(ChatLine {
                seq: message["seq"].as_i64().unwrap_or(0),
                sender: text(&message["sender_id"]),
                body: text(&message["body"]),
                time: message["created_at"].as_i64().unwrap_or(0),
            });
        }
    }

    // Direct messages: every conversation this account is in, keyed `@nick`.
    let my_id = snap
        .accounts
        .iter()
        .find(|a| a.nick.eq_ignore_ascii_case(&cfg.nick))
        .map(|a| a.id.clone())
        .unwrap_or_else(|| cfg.nick.clone());
    loop {
        let fetched = get(
            client,
            cfg,
            &format!("/v1/messages?dm=all&after_seq={}&limit=500", snap.dm_seq),
        )
        .await?;
        let batch = fetched["messages"].as_array().cloned().unwrap_or_default();
        for message in &batch {
            let seq = message["seq"].as_i64().unwrap_or(0);
            snap.dm_seq = snap.dm_seq.max(seq);
            let Some(peer) = dm_peer(&text(&message["channel"]), &my_id) else {
                continue;
            };
            let nick = snap
                .accounts
                .iter()
                .find(|a| a.id.eq_ignore_ascii_case(&peer))
                .map(|a| a.nick.clone())
                .unwrap_or(peer);
            snap.messages
                .entry(format!("@{nick}"))
                .or_default()
                .push(ChatLine {
                    seq,
                    sender: text(&message["sender_id"]),
                    body: text(&message["body"]),
                    time: message["created_at"].as_i64().unwrap_or(0),
                });
        }
        if batch.len() < 500 {
            break;
        }
    }
    let dms: Vec<String> = snap
        .messages
        .keys()
        .filter(|name| name.starts_with('@'))
        .cloned()
        .collect();
    snap.channels.extend(dms);

    // Keep whatever an action wrote while this refresh was in flight.
    let mut guard = shared.lock().expect("snapshot mutex poisoned");
    snap.notice = guard.notice.clone();
    snap.detail = guard.detail.clone();
    snap.models = guard.models.clone();
    snap.error = None;
    *guard = snap;
    Ok(())
}

async fn load_task(
    client: &reqwest::Client,
    cfg: &TuiConfig,
    id: &str,
    pm: Option<&str>,
) -> Result<Vec<String>> {
    let body = get(client, cfg, &format!("/v1/tasks/{id}")).await?;
    let parent = match body["task"]["parent_task_id"].as_str() {
        // A parent we cannot read still shows by its id.
        Some(parent) => get(client, cfg, &format!("/v1/tasks/{parent}")).await.ok(),
        None => None,
    };
    Ok(task_detail_lines(
        &body,
        parent.as_ref().map(|p| &p["task"]),
        pm,
    ))
}

/// The other side of a stored DM channel `@dm:<id>:<id>`, as a principal id.
pub fn dm_peer(channel: &str, me: &str) -> Option<String> {
    let (left, right) = channel.strip_prefix("@dm:")?.split_once(':')?;
    let peer = if left.eq_ignore_ascii_case(me) {
        right
    } else {
        left
    };
    Some(peer.to_string())
}

/// Who reviews a task, and why: the parent task's lead or the project PM.
fn reviewer_text(reviewer: Option<&str>, parent: Option<&Value>, pm: Option<&str>) -> String {
    let Some(reviewer) = reviewer else {
        return "-".to_string();
    };
    let lead = parent.and_then(|p| p["assignee"].as_str());
    let why = if lead.is_some_and(|lead| lead.eq_ignore_ascii_case(reviewer)) {
        " (lead)"
    } else if pm.is_some_and(|pm| pm.eq_ignore_ascii_case(reviewer)) {
        " (PM)"
    } else {
        ""
    };
    format!("@{reviewer}{why}")
}

/// The lines of the task details window. `parent` is the parent task when it
/// could be loaded, and `pm` is the project manager.
pub fn task_detail_lines(body: &Value, parent: Option<&Value>, pm: Option<&str>) -> Vec<String> {
    let task = &body["task"];
    let or_dash = |value: &Value| value.as_str().unwrap_or("-").to_string();
    let parent_text = match task["parent_task_id"].as_str() {
        None => "-".to_string(),
        Some(id) => match parent.and_then(|p| p["title"].as_str()) {
            Some(title) => format!("{id} \"{title}\""),
            None => id.to_string(),
        },
    };
    let mut lines = vec![
        format!("{}  [{}]", text(&task["title"]), text(&task["kind"])),
        format!(
            "{}   status {}   assignee {}",
            text(&task["channel"]),
            text(&task["status"]),
            or_dash(&task["assignee"])
        ),
        format!(
            "repo {}   branch {}   issue {}",
            or_dash(&task["repo"]),
            or_dash(&task["branch"]),
            or_dash(&task["issue_url"])
        ),
        format!(
            "reviewer {}   parent {parent_text}",
            reviewer_text(task["reviewer"].as_str(), parent, pm)
        ),
        String::new(),
    ];
    if let Some(description) = task["body"].as_str() {
        lines.push(description.to_string());
        lines.push(String::new());
    }
    if let Some(question) = body["question"].as_object() {
        lines.push(format!(
            "WAITING: @{} asks: {}",
            text(&question["asked_by"]),
            text(&question["question"])
        ));
        lines.push(String::new());
    }
    lines.push("History".to_string());
    for event in body["events"].as_array().into_iter().flatten() {
        let note = event["note"]
            .as_str()
            .map(|n| format!(" - {n}"))
            .unwrap_or_default();
        lines.push(format!(
            "  {} {} -> {} by @{}{note}",
            text(&event["action"]),
            or_dash(&event["from_status"]),
            or_dash(&event["to_status"]),
            text(&event["actor_id"])
        ));
    }
    lines
}

async fn load_models(
    client: &reqwest::Client,
    cfg: &TuiConfig,
    template: &str,
) -> Result<Vec<ModelItem>> {
    let key = cfg
        .owner_key
        .as_deref()
        .ok_or_else(|| anyhow!("the owner key is not on this machine"))?;
    let body = json!({"invite": key, "template": template});
    let listed = request(client, cfg, "/v1/owner/models", Some(&body)).await?;
    Ok(listed["models"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|m| (text(&m["id"]), m["label"].as_str().map(str::to_owned)))
        .filter(|(id, _)| !id.is_empty())
        .collect())
}

/// Create the account, then save its token as the airc profile named after
/// it, so `airc --profile <nick>` works for the agent straight away.
async fn create_account(client: &reqwest::Client, cfg: &TuiConfig, mut body: Value) -> (bool, String) {
    match cfg.owner_key.as_deref() {
        Some(key) => body["invite"] = json!(key),
        None => return (false, "the owner key is not on this machine".to_string()),
    }
    let created = match request(client, cfg, "/v1/owner/accounts", Some(&body)).await {
        Ok(created) => created,
        Err(err) => return (false, format!("{err}")),
    };
    let account = &created["account"];
    let nick = text(&account["principal"]["nick"]);
    let channel = text(&account["channel"]);
    match save_profile(&config_dir(), &nick, &cfg.server, &text(&account["token"])) {
        Ok(()) => (true, format!("Created @{nick} in {channel}; its airc profile is saved")),
        Err(err) => (
            false,
            format!("Created @{nick} in {channel}, but its airc profile was not saved: {err:#}"),
        ),
    }
}

/// Add or replace one profile in `profiles.json`, leaving the default and
/// every other profile as they were.
pub fn save_profile(dir: &std::path::Path, nick: &str, server: &str, token: &str) -> Result<()> {
    if nick.is_empty() || token.is_empty() {
        return Err(anyhow!("the hub did not return the new account's token"));
    }
    let path = dir.join("profiles.json");
    let mut data: Value = match std::fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => json!({"default": nick}),
        Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
    };
    if !data.is_object() {
        return Err(anyhow!("{} is not a profile file", path.display()));
    }
    if !data["profiles"].is_object() {
        data["profiles"] = json!({});
    }
    data["profiles"][nick] = json!({"server": server, "token": token, "nick": nick});
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let temp = dir.join("profiles.json.tmp");
    std::fs::write(&temp, serde_json::to_string_pretty(&data)? + "\n")
        .with_context(|| format!("write {}", temp.display()))?;
    std::fs::rename(&temp, &path).with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

/// Send one request for an action; returns whether it worked and the line
/// to show the user.
async fn run_api(
    client: &reqwest::Client,
    cfg: &TuiConfig,
    path: String,
    mut body: Value,
    owner: bool,
    done: String,
) -> (bool, String) {
    if owner {
        match cfg.owner_key.as_deref() {
            Some(key) => body["invite"] = json!(key),
            None => return (false, "the owner key is not on this machine".to_string()),
        }
    }
    match request(client, cfg, &path, Some(&body)).await {
        Ok(_) => (true, done),
        Err(err) => (false, format!("{err}")),
    }
}

async fn fetch_loop(
    cfg: TuiConfig,
    shared: Arc<Mutex<Snapshot>>,
    mut commands: UnboundedReceiver<Action>,
    stop: Arc<AtomicBool>,
) {
    let client = reqwest::Client::new();
    let set_notice = |ok: bool, message: String| {
        shared.lock().expect("snapshot mutex poisoned").notice =
            Some((ok, message, Instant::now()));
    };
    while !stop.load(Ordering::Relaxed) {
        tokio::select! {
            command = commands.recv() => {
                match command {
                    Some(Action::Send { target, body }) => {
                        let payload = json!({"target": target, "body": body});
                        if let Err(err) = request(&client, &cfg, "/v1/messages", Some(&payload)).await {
                            set_notice(false, format!("send failed: {err}"));
                        }
                    }
                    Some(Action::Api { path, body, owner, done }) => {
                        let (ok, message) = run_api(&client, &cfg, path, body, owner, done).await;
                        set_notice(ok, message);
                    }
                    Some(Action::Many(steps)) => {
                        let mut outcome = (true, String::new());
                        for step in steps {
                            if let Action::Api { path, body, owner, done } = step {
                                outcome = run_api(&client, &cfg, path, body, owner, done).await;
                                if !outcome.0 {
                                    break;
                                }
                            }
                        }
                        set_notice(outcome.0, outcome.1);
                    }
                    Some(Action::LoadModels(template)) => {
                        shared.lock().expect("snapshot mutex poisoned").models = None;
                        let listed = load_models(&client, &cfg, &template).await;
                        shared.lock().expect("snapshot mutex poisoned").models =
                            Some((template, listed.map_err(|err| format!("{err}"))));
                    }
                    Some(Action::CreateAccount { body }) => {
                        let (ok, message) = create_account(&client, &cfg, body).await;
                        set_notice(ok, message);
                    }
                    Some(Action::LoadTask(id)) => {
                        // Knowing the PM lets the details say why someone reviews.
                        let pm = {
                            let snap = shared.lock().expect("snapshot mutex poisoned");
                            snap.projects
                                .iter()
                                .find(|p| p.tasks.iter().any(|t| t.id == id))
                                .and_then(|p| p.pm.clone())
                        };
                        let lines = load_task(&client, &cfg, &id, pm.as_deref())
                            .await
                            .unwrap_or_else(|err| vec![format!("Could not load the task: {err}")]);
                        shared.lock().expect("snapshot mutex poisoned").detail = Some((id, lines));
                    }
                    Some(_) => {}
                    None => break,
                }
            }
            _ = tokio::time::sleep(REFRESH_EVERY) => {}
        }
        if let Err(err) = refresh(&client, &cfg, &shared).await {
            shared.lock().expect("snapshot mutex poisoned").error = Some(format!("{err:#}"));
        }
    }
}

// ---------------------------------------------------------------- input

/// Complete the `@name` being typed at the end of `input`.
pub fn complete_mention(input: &str, accounts: &[Account]) -> Option<String> {
    let start = input.rfind('@')?;
    let partial = &input[start + 1..];
    if partial.contains(char::is_whitespace) {
        return None;
    }
    let lowered = partial.to_ascii_lowercase();
    let mut matches: Vec<&str> = accounts
        .iter()
        .map(|a| a.nick.as_str())
        .filter(|nick| nick.to_ascii_lowercase().starts_with(&lowered))
        .collect();
    matches.sort_unstable();
    match matches.as_slice() {
        [] => None,
        [only] => Some(format!("{}@{only} ", &input[..start])),
        many => {
            // Extend to the longest prefix every candidate shares.
            let first = many[0];
            let shared = first
                .char_indices()
                .take_while(|(i, c)| {
                    many.iter().all(|m| {
                        m.get(*i..)
                            .and_then(|rest| rest.chars().next())
                            .is_some_and(|other| other.eq_ignore_ascii_case(c))
                    })
                })
                .count();
            (shared > partial.len()).then(|| format!("{}@{}", &input[..start], &first[..shared]))
        }
    }
}

fn picker(title: impl Into<String>, items: Vec<(String, String)>, flow: Flow) -> Option<Modal> {
    (!items.is_empty()).then(|| Modal::Picker {
        title: title.into(),
        items,
        idx: 0,
        flow,
    })
}

fn prompt(title: impl Into<String>, flow: Flow) -> Option<Modal> {
    Some(Modal::Prompt {
        title: title.into(),
        text: String::new(),
        flow,
    })
}

fn prompt_with(title: impl Into<String>, text: impl Into<String>, flow: Flow) -> Option<Modal> {
    Some(Modal::Prompt {
        title: title.into(),
        text: text.into(),
        flow,
    })
}

/// A model id turned into a usable account name: letters, digits, `.`, `_`
/// and `-`, at most 32 characters.
pub fn nick_for_model(model: &str) -> String {
    let nick: String = model
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '-' })
        .take(32)
        .collect();
    nick.trim_matches('-').to_string()
}

/// The model picker's rows: `None` while the hub is still listing them.
/// The last row always lets you type a model the list does not show.
pub fn model_items(snap: &Snapshot, template: &str) -> Option<(Vec<(String, String)>, Option<String>)> {
    let (listed_for, listed) = snap.models.as_ref()?;
    if listed_for != template {
        return None;
    }
    let (mut items, problem) = match listed {
        Ok(models) => (
            models
                .iter()
                .map(|(id, label)| {
                    let shown = match label {
                        Some(label) => format!("{id}  {label}"),
                        None => id.clone(),
                    };
                    (shown, id.clone())
                })
                .collect::<Vec<_>>(),
            None,
        ),
        Err(err) => (Vec::new(), Some(err.clone())),
    };
    items.push(("Type a model name...".to_string(), TYPE_MODEL.to_string()));
    Some((items, problem))
}

fn choices(values: &[&str]) -> Vec<(String, String)> {
    values
        .iter()
        .map(|v| (v.to_string(), v.to_string()))
        .collect()
}

fn member_choices(snap: &Snapshot, channel: &str, none_label: &str) -> Vec<(String, String)> {
    let mut items: Vec<(String, String)> = snap
        .projects
        .iter()
        .find(|p| p.channel == channel)
        .into_iter()
        .flat_map(|p| p.members.iter())
        .map(|m| (format!("@{}  ({})", m.nick, m.role), m.nick.clone()))
        .collect();
    items.push((none_label.to_string(), String::new()));
    items
}

fn optional(value: &str) -> Value {
    if value.trim().is_empty() {
        Value::Null
    } else {
        json!(value.trim())
    }
}

fn api(path: String, body: Value, done: &str) -> Option<Action> {
    Some(Action::Api {
        path,
        body,
        owner: false,
        done: done.to_string(),
    })
}

/// Take a pop-up's answer one step further: open the next pop-up, or
/// return the request to send.
fn advance(app: &mut App, flow: Flow, value: String, snap: &Snapshot) -> Option<Action> {
    match flow {
        Flow::OpenDm => {
            open_dm(app, &value);
            None
        }
        Flow::NewTitle { channel } => {
            if !value.trim().is_empty() {
                app.modal = picker(
                    "Assign to",
                    member_choices(snap, &channel, "(nobody yet)"),
                    Flow::NewAssignee {
                        channel,
                        title: value.trim().to_string(),
                    },
                );
            }
            None
        }
        Flow::NewAssignee { channel, title } => {
            app.modal = picker(
                "Kind",
                choices(KIND_CHOICES),
                Flow::NewKind {
                    channel,
                    title,
                    assignee: value,
                },
            );
            None
        }
        Flow::NewKind {
            channel,
            title,
            assignee,
        } => {
            app.modal = prompt(
                "Description (optional, Enter to skip)",
                Flow::NewBody {
                    channel,
                    title,
                    assignee,
                    kind: value,
                },
            );
            None
        }
        Flow::NewBody {
            channel,
            title,
            assignee,
            kind,
        } => api(
            "/v1/tasks".to_string(),
            json!({
                "channel": channel, "title": title, "kind": kind,
                "assignee": optional(&assignee), "body": optional(&value),
            }),
            "Task created",
        ),
        Flow::Assign { task_id } => api(
            format!("/v1/tasks/{task_id}/assign"),
            json!({"assignee": optional(&value)}),
            "Task reassigned",
        ),
        Flow::Status { task_id } => {
            app.modal = prompt(
                "Note (optional, Enter to skip)",
                Flow::StatusNote {
                    task_id,
                    status: value,
                },
            );
            None
        }
        Flow::StatusNote { task_id, status } => api(
            format!("/v1/tasks/{task_id}/status"),
            json!({"status": status, "note": optional(&value)}),
            "Task status changed",
        ),
        Flow::SetPm { channel } => api(
            "/v1/projects/pm".to_string(),
            json!({"channel": channel, "nick": value}),
            "Project manager set",
        ),
        Flow::AddMember { channel } if value == NEW_ACCOUNT => {
            app.modal = if snap.templates.is_empty() {
                picker(
                    "No client templates on the hub yet",
                    vec![("(close)".to_string(), String::new())],
                    Flow::NewAccountClient { channel },
                )
            } else {
                picker(
                    format!("New account for {channel}: client"),
                    snap.templates
                        .iter()
                        .map(|t| {
                            let missing = if t.installed { "" } else { "  (not installed)" };
                            (format!("{}{missing}", t.name), t.id.clone())
                        })
                        .collect(),
                    Flow::NewAccountClient { channel },
                )
            };
            None
        }
        Flow::NewAccountClient { channel } => {
            if value.is_empty() {
                return None;
            }
            app.modal = Some(Modal::Models {
                channel,
                template: value.clone(),
                idx: 0,
            });
            Some(Action::LoadModels(value))
        }
        Flow::NewAccountModel { channel, template } => {
            if value == TYPE_MODEL {
                app.modal = prompt(
                    "Model name, exactly as the client expects it",
                    Flow::NewAccountModel { channel, template },
                );
                return None;
            }
            let model = value.trim().to_string();
            if model.is_empty() {
                return None;
            }
            app.modal = prompt_with(
                format!("Account name for {model}"),
                nick_for_model(&model),
                Flow::NewAccountName { channel, template, model },
            );
            None
        }
        Flow::NewAccountName { channel, template, model } => {
            let nick = value.trim().trim_start_matches('@').to_string();
            if nick.is_empty() {
                return None;
            }
            app.modal = picker(
                format!("Role for @{nick} in {channel}"),
                choices(ROLE_CHOICES),
                Flow::NewAccountRole { channel, template, model, nick },
            );
            None
        }
        Flow::NewAccountRole { channel, template, model, nick } => Some(Action::CreateAccount {
            body: json!({
                "template": template, "model": model, "nick": nick,
                "channel": channel, "role": value,
            }),
        }),
        Flow::AddMember { channel } => {
            app.modal = picker(
                format!("Role for @{value}"),
                choices(ROLE_CHOICES),
                Flow::AddMemberRole {
                    channel,
                    nick: value,
                },
            );
            None
        }
        Flow::AddMemberRole { channel, nick } => {
            let done = format!("@{nick} is now {value} in {channel}");
            api(
                "/v1/projects/members".to_string(),
                json!({"channel": channel, "nick": nick, "role": value}),
                &done,
            )
        }
        Flow::AnswerPick => {
            let question = snap
                .waiting
                .iter()
                .find(|w| w.task_id == value)
                .map(|w| w.question.clone())
                .unwrap_or_default();
            app.modal = prompt(
                format!("Answer: {question}"),
                Flow::AnswerText { task_id: value },
            );
            None
        }
        Flow::AnswerText { task_id } => {
            if value.trim().is_empty() {
                return None;
            }
            api(
                format!("/v1/tasks/{task_id}/answer"),
                json!({"answer": value.trim()}),
                "Answer sent",
            )
        }
        Flow::OpenSprint { channel } => api(
            "/v1/rooms".to_string(),
            json!({"project": channel, "title": optional(&value)}),
            "Sprint opened",
        ),
        Flow::NewProjectName => {
            if !value.trim().is_empty() {
                app.modal = prompt(
                    "Repo folder on this machine (optional, Enter to skip)",
                    Flow::NewProjectRepo {
                        name: value.trim().to_string(),
                    },
                );
            }
            None
        }
        Flow::NewProjectRepo { name } => {
            let slug = name
                .trim_start_matches('#')
                .to_ascii_lowercase()
                .replace(' ', "-");
            let channel = format!("#{slug}");
            let mut steps = vec![Action::Api {
                path: "/v1/projects".to_string(),
                body: json!({"channel": channel, "name": name}),
                owner: false,
                done: format!("Project {channel} created"),
            }];
            if !value.trim().is_empty() {
                steps.push(Action::Api {
                    path: "/v1/gits".to_string(),
                    body: json!({"name": slug, "channel": channel, "local_path": value.trim()}),
                    owner: false,
                    done: format!("Project {channel} created and its repo attached"),
                });
            }
            Some(Action::Many(steps))
        }
        Flow::TestAgent => Some(Action::Api {
            path: "/v1/owner/wake".to_string(),
            body: json!({
                "action": "test", "nick": value,
                "message": "Connectivity check from the owner. Reply on the hub in one short line that you are reachable, and do nothing else.",
            }),
            owner: true,
            done: format!("Test wake sent to @{value}"),
        }),
        Flow::ToggleLauncher => {
            let (nick, action) = value.split_once('|')?;
            Some(Action::Api {
                path: "/v1/owner/wake".to_string(),
                body: json!({"action": action, "nick": nick}),
                owner: true,
                done: format!("Launcher for @{nick}: {action}d"),
            })
        }
    }
}

fn handle_modal(app: &mut App, code: KeyCode, snap: &Snapshot) -> Option<Action> {
    let modal = app.modal.take()?;
    match modal {
        Modal::Picker {
            title,
            items,
            mut idx,
            flow,
        } => match code {
            KeyCode::Esc => None,
            KeyCode::Enter => {
                let value = items.get(idx).map(|(_, v)| v.clone()).unwrap_or_default();
                advance(app, flow, value, snap)
            }
            other => {
                match other {
                    KeyCode::Up | KeyCode::Char('k') => idx = idx.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => idx = (idx + 1).min(items.len() - 1),
                    _ => {}
                }
                app.modal = Some(Modal::Picker {
                    title,
                    items,
                    idx,
                    flow,
                });
                None
            }
        },
        Modal::Prompt {
            title,
            mut text,
            flow,
        } => match code {
            KeyCode::Esc => None,
            KeyCode::Enter => advance(app, flow, text, snap),
            other => {
                match other {
                    KeyCode::Backspace => {
                        text.pop();
                    }
                    KeyCode::Char(c) => text.push(c),
                    _ => {}
                }
                app.modal = Some(Modal::Prompt { title, text, flow });
                None
            }
        },
        Modal::Models {
            channel,
            template,
            mut idx,
        } => {
            let items = model_items(snap, &template).map(|(items, _)| items);
            match (code, items) {
                (KeyCode::Esc, _) => None,
                (KeyCode::Enter, Some(items)) => {
                    let value = items.get(idx).map(|(_, v)| v.clone()).unwrap_or_default();
                    advance(app, Flow::NewAccountModel { channel, template }, value, snap)
                }
                (other, items) => {
                    let last = items.map(|i| i.len().saturating_sub(1)).unwrap_or(0);
                    match other {
                        KeyCode::Up | KeyCode::Char('k') => idx = idx.saturating_sub(1),
                        KeyCode::Down | KeyCode::Char('j') => idx = (idx + 1).min(last),
                        _ => {}
                    }
                    app.modal = Some(Modal::Models { channel, template, idx });
                    None
                }
            }
        }
        Modal::Detail { .. } | Modal::Help => None,
    }
}

pub fn handle_key(
    app: &mut App,
    code: KeyCode,
    modifiers: KeyModifiers,
    snap: &Snapshot,
) -> Option<Action> {
    if modifiers.contains(KeyModifiers::CONTROL) && matches!(code, KeyCode::Char('c')) {
        return Some(Action::Quit);
    }
    if app.modal.is_some() {
        return handle_modal(app, code, snap);
    }
    if modifiers.contains(KeyModifiers::CONTROL) && matches!(code, KeyCode::Char('d')) {
        let people: Vec<(String, String)> = snap
            .accounts
            .iter()
            .filter(|a| !a.nick.eq_ignore_ascii_case(&snap.me))
            .map(|a| (format!("@{}  {}", a.nick, a.role), a.nick.clone()))
            .collect();
        app.modal = picker("Direct message", people, Flow::OpenDm);
        return None;
    }
    match app.view {
        View::Home => handle_home_key(app, code, snap),
        View::Dashboard => handle_dashboard_key(app, code, snap),
        View::Chat => handle_chat_key(app, code, snap),
    }
}

fn handle_home_key(app: &mut App, code: KeyCode, snap: &Snapshot) -> Option<Action> {
    match code {
        KeyCode::Down | KeyCode::Char('j') => {
            app.home_cursor = (app.home_cursor + 1).min(snap.projects.len().saturating_sub(1));
        }
        KeyCode::Up | KeyCode::Char('k') => app.home_cursor = app.home_cursor.saturating_sub(1),
        KeyCode::Enter | KeyCode::Char('d') => {
            if let Some(project) = snap.projects.get(app.home_cursor) {
                app.project = Some(project.channel.clone());
                app.cursor = 0;
                app.scroll = 0;
                app.view = View::Dashboard;
            }
        }
        KeyCode::Char('n') | KeyCode::Char('N') => {
            app.modal = prompt("New project: name", Flow::NewProjectName);
        }
        KeyCode::Char('t') => {
            app.modal = picker(
                "Send a test wake (uses that agent's quota)",
                snap.accounts
                    .iter()
                    .filter(|a| a.launcher.is_some())
                    .map(|a| (format!("@{}", a.nick), a.nick.clone()))
                    .collect(),
                Flow::TestAgent,
            );
        }
        // Task actions belong to the dashboard.
        KeyCode::Char('a') | KeyCode::Char('s') | KeyCode::Esc => {}
        other => return handle_dashboard_key(app, other, snap),
    }
    None
}

fn handle_dashboard_key(app: &mut App, code: KeyCode, snap: &Snapshot) -> Option<Action> {
    let last = rows(snap, app.project.as_deref())
        .len()
        .saturating_sub(1);
    match code {
        KeyCode::Char('q') | KeyCode::Char('Q') => return Some(Action::Quit),
        KeyCode::Esc => app.view = View::Home,
        KeyCode::Char('c') | KeyCode::Char('C') => {
            // Chat opens in the room of the project you are looking at.
            let room = if app.view == View::Home {
                snap.projects.get(app.home_cursor).map(|p| p.channel.clone())
            } else {
                app.project.clone()
            };
            if let Some(index) = room.and_then(|r| snap.channels.iter().position(|c| *c == r)) {
                app.channel_idx = index;
                app.chat_channel = None;
            }
            app.chat_scroll = 0;
            app.view = View::Chat;
        }
        KeyCode::Char('r') | KeyCode::Char('R') => return Some(Action::Refresh),
        KeyCode::Char('?') | KeyCode::Char('h') => app.modal = Some(Modal::Help),
        KeyCode::Down | KeyCode::Char('j') => app.cursor = (app.cursor + 1).min(last),
        KeyCode::Up | KeyCode::Char('k') => app.cursor = app.cursor.saturating_sub(1),
        KeyCode::PageDown => app.cursor = (app.cursor + 10).min(last),
        KeyCode::PageUp => app.cursor = app.cursor.saturating_sub(10),
        KeyCode::Enter => {
            if let Some(task) = selected_task(app, snap) {
                app.modal = Some(Modal::Detail {
                    task_id: task.id.clone(),
                });
                return Some(Action::LoadTask(task.id.clone()));
            }
        }
        KeyCode::Char('n') => {
            if let Some(project) = selected_project(app, snap) {
                app.modal = prompt(
                    format!("New task in {}: title", project.channel),
                    Flow::NewTitle {
                        channel: project.channel.clone(),
                    },
                );
            }
        }
        KeyCode::Char('a') => {
            if let (Some(task), Some(project)) =
                (selected_task(app, snap), selected_project(app, snap))
            {
                app.modal = picker(
                    format!("Assign: {}", task.title),
                    member_choices(snap, &project.channel, "(return to the pool)"),
                    Flow::Assign {
                        task_id: task.id.clone(),
                    },
                );
            }
        }
        KeyCode::Char('s') => {
            if let Some(task) = selected_task(app, snap) {
                app.modal = picker(
                    format!("Move from {}: {}", task.status, task.title),
                    choices(STATUS_CHOICES),
                    Flow::Status {
                        task_id: task.id.clone(),
                    },
                );
            }
        }
        KeyCode::Char('p') => {
            if let Some(project) = selected_project(app, snap) {
                app.modal = picker(
                    format!("Project manager of {}", project.channel),
                    snap.accounts
                        .iter()
                        .map(|a| (format!("@{}  ({})", a.nick, a.role), a.nick.clone()))
                        .collect(),
                    Flow::SetPm {
                        channel: project.channel.clone(),
                    },
                );
            }
        }
        KeyCode::Char('m') => {
            if let Some(project) = selected_project(app, snap) {
                // Every account, members first showing their role (choosing
                // one changes it), then the rest, then a new account.
                let role_of = |nick: &str| {
                    project
                        .members
                        .iter()
                        .find(|m| m.nick.eq_ignore_ascii_case(nick))
                        .map(|m| m.role.clone())
                };
                let mut items: Vec<(String, String)> = snap
                    .accounts
                    .iter()
                    .filter_map(|a| {
                        role_of(&a.nick).map(|role| {
                            (format!("@{}  ({role}, change role)", a.nick), a.nick.clone())
                        })
                    })
                    .collect();
                items.extend(
                    snap.accounts
                        .iter()
                        .filter(|a| role_of(&a.nick).is_none())
                        .map(|a| (format!("@{}  (add)", a.nick), a.nick.clone())),
                );
                items.push((
                    "+ new account (client and model)".to_string(),
                    NEW_ACCOUNT.to_string(),
                ));
                app.modal = picker(
                    format!("Members of {}", project.channel),
                    items,
                    Flow::AddMember {
                        channel: project.channel.clone(),
                    },
                );
            }
        }
        KeyCode::Char('o') => {
            if let Some(project) = selected_project(app, snap) {
                app.modal = prompt(
                    format!(
                        "Open the next sprint of {}: title (optional)",
                        project.channel
                    ),
                    Flow::OpenSprint {
                        channel: project.channel.clone(),
                    },
                );
            }
        }
        KeyCode::Char('w') => {
            app.modal = picker(
                "Answer which question?",
                snap.waiting
                    .iter()
                    .map(|w| {
                        (
                            format!("{} @{}: {}", w.channel, w.asked_by, w.question),
                            w.task_id.clone(),
                        )
                    })
                    .collect(),
                Flow::AnswerPick,
            );
        }
        KeyCode::Char('l') => {
            app.modal = picker(
                "Switch a launcher on or off",
                snap.launchers
                    .iter()
                    .map(|l| {
                        let (state, action) = if l.enabled {
                            ("on ", "disable")
                        } else {
                            ("off", "enable")
                        };
                        (
                            format!("{state}  @{}  ({})  -> {action}", l.nick, l.mode),
                            format!("{}|{action}", l.nick),
                        )
                    })
                    .collect(),
                Flow::ToggleLauncher,
            );
        }
        _ => {}
    }
    None
}

fn handle_chat_key(app: &mut App, code: KeyCode, snap: &Snapshot) -> Option<Action> {
    match code {
        KeyCode::Esc => app.view = View::Dashboard,
        KeyCode::Up => {
            app.channel_idx = app.channel_idx.saturating_sub(1);
            app.chat_channel = None;
            app.chat_scroll = 0;
        }
        KeyCode::Down => {
            if app.channel_idx + 1 < chat_rooms(app, snap).len() {
                app.channel_idx += 1;
            }
            app.chat_channel = None;
            app.chat_scroll = 0;
        }
        KeyCode::PageUp => {
            app.chat_scroll = app
                .chat_scroll
                .saturating_add(usize::from(app.chat_area.height).max(1))
                .min(app.chat_max_scroll);
        }
        KeyCode::PageDown => {
            app.chat_scroll = app
                .chat_scroll
                .saturating_sub(usize::from(app.chat_area.height).max(1));
        }
        KeyCode::Home => app.chat_scroll = app.chat_max_scroll,
        KeyCode::End => app.chat_scroll = 0,
        KeyCode::Backspace => {
            app.input.pop();
            app.chat_scroll = 0;
        }
        KeyCode::Tab => {
            if let Some(done) = complete_mention(&app.input, &snap.accounts) {
                app.input = done;
                app.chat_scroll = 0;
            }
        }
        KeyCode::Enter => {
            let body = app.input.trim().to_string();
            app.input.clear();
            app.chat_scroll = 0;
            let rooms = chat_rooms(app, snap);
            if let (false, Some(target)) = (body.is_empty(), rooms.get(app.channel_idx)) {
                return Some(Action::Send {
                    target: target.clone(),
                    body,
                });
            }
        }
        KeyCode::Char(c) => {
            app.input.push(c);
            app.chat_scroll = 0;
        }
        _ => {}
    }
    None
}

pub fn handle_click(app: &mut App, column: u16, row: u16) -> Option<Action> {
    if app.modal.is_some() {
        return None;
    }
    if row == 0 {
        let within = |label: &str| {
            let start = TABS.find(label).unwrap_or(0) as u16;
            column >= start && column < start + label.len() as u16
        };
        if within("[H]ome") {
            app.view = View::Home;
        } else if within("[D]ashboard") {
            app.view = View::Dashboard;
        } else if within("[C]hat") {
            app.view = View::Chat;
        } else if within("[Q]uit") {
            return Some(Action::Quit);
        }
        return None;
    }
    match app.view {
        View::Chat if column < CHANNEL_PANE_WIDTH => {
            if let Some((_, index)) = app.channel_rows.iter().find(|(y, _)| *y == row) {
                app.channel_idx = *index;
                app.chat_channel = None;
                app.chat_scroll = 0;
            }
        }
        View::Chat if app.people_area.contains((column, row).into()) => {
            if let Some((_, nick)) = app.people_rows.iter().find(|(y, _)| *y == row) {
                let nick = nick.clone();
                open_dm(app, &nick);
            }
        }
        View::Dashboard => {
            if let Some((_, index)) = app.task_rows.iter().find(|(y, _)| *y == row) {
                app.cursor = *index;
            }
        }
        View::Home => {
            if let Some((_, index)) = app.home_rows.iter().find(|(y, _)| *y == row) {
                app.home_cursor = *index;
            }
        }
        _ => {}
    }
    None
}

pub fn handle_mouse(app: &mut App, mouse: MouseEvent) -> Option<Action> {
    if app.modal.is_some() {
        return None;
    }
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => handle_click(app, mouse.column, mouse.row),
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
            if app.view == View::Chat
                && app.chat_area.contains((mouse.column, mouse.row).into()) =>
        {
            app.chat_scroll = match mouse.kind {
                MouseEventKind::ScrollUp => {
                    app.chat_scroll.saturating_add(3).min(app.chat_max_scroll)
                }
                _ => app.chat_scroll.saturating_sub(3),
            };
            None
        }
        _ => None,
    }
}

// ---------------------------------------------------------------- drawing

fn status_color(status: &str) -> Color {
    match status {
        "blocked" => Color::Red,
        "testing" => Color::Magenta,
        "review" => Color::Blue,
        "changes_requested" => Color::Yellow,
        "in_progress" | "claimed" => Color::Green,
        _ => Color::Gray,
    }
}

fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

fn dim() -> Style {
    Style::default().fg(Color::DarkGray)
}

fn clock(unix: i64) -> String {
    let day = unix.rem_euclid(86_400);
    format!("{:02}:{:02}", day / 3600, (day % 3600) / 60)
}

pub fn draw(frame: &mut Frame, app: &mut App, snap: &Snapshot) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    // Rooms that first appear after this count their messages as unread.
    if app.seen_floor.is_none() {
        app.seen_floor = Some(
            snap.messages
                .values()
                .filter_map(|lines| lines.last())
                .map(|line| line.seq)
                .max()
                .unwrap_or(0),
        );
    }
    match app.view {
        View::Home => draw_home(frame, app, snap, body),
        View::Dashboard => draw_dashboard(frame, app, snap, body),
        View::Chat => draw_chat(frame, app, snap, body),
    }

    let who = format!(
        "  CAMS  {}  you: @{} ({})",
        snap.server,
        snap.me,
        if snap.my_role.is_empty() {
            "?"
        } else {
            &snap.my_role
        }
    );
    let mut header_spans = vec![
        Span::styled(TABS, bold().fg(Color::Black).bg(Color::Cyan)),
        Span::raw(who),
    ];
    let dm_unread: usize = snap
        .messages
        .keys()
        .filter(|room| room.starts_with('@'))
        .map(|room| unread(app, snap, room))
        .sum();
    if dm_unread > 0 {
        header_spans.push(Span::styled(
            format!("  [{dm_unread} new direct]"),
            bold().fg(Color::Yellow),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(header_spans)), header);

    let hints: &[(&str, &str)] = match app.view {
        View::Home => &[
            ("Enter", "open"),
            ("n", "new project"),
            ("m", "member"),
            ("p", "PM"),
            ("o", "sprint"),
            ("t", "test"),
            ("w", "answer"),
            ("c", "chat"),
            ("?", "help"),
            ("q", "quit"),
        ],
        View::Dashboard => &[
            ("Enter", "details"),
            ("n", "new"),
            ("a", "assign"),
            ("s", "status"),
            ("w", "answer"),
            ("o", "sprint"),
            ("c", "chat"),
            ("Esc", "home"),
            ("?", "help"),
            ("q", "quit"),
        ],
        View::Chat => &[
            ("Enter", "send"),
            ("Tab", "complete @name"),
            ("Up/Down", "room"),
            ("^D", "direct message"),
            ("PgUp/PgDn", "history"),
            ("End", "latest"),
            ("Esc", "dashboard"),
        ],
    };
    let recent_notice = snap
        .notice
        .as_ref()
        .filter(|(_, _, at)| at.elapsed() < NOTICE_FOR);
    let footer_line = match (&snap.error, recent_notice) {
        (Some(err), _) => Line::from(Span::styled(
            format!(" {err}"),
            Style::default().fg(Color::Red),
        )),
        (None, Some((ok, message, _))) => Line::from(Span::styled(
            format!(" {message}"),
            bold().fg(if *ok { Color::Green } else { Color::Red }),
        )),
        (None, None) => hint_line(hints),
    };
    frame.render_widget(Paragraph::new(footer_line), footer);

    if let Some(modal) = app.modal.clone() {
        draw_modal(frame, &modal, snap, body);
    }
}

fn trio_height(snap: &Snapshot, area: Rect) -> u16 {
    (snap.accounts.len() as u16 + 2).clamp(9, (area.height / 2 + 1).max(9))
}

/// One word on whether the hub can start this account's agent. Reading it
/// costs nothing: it comes from the launcher settings and the last launch.
pub fn account_status(account: &Account) -> (String, Color) {
    let status = |text: &str, color: Color| (text.to_string(), color);
    if account.running {
        return status("working", Color::Cyan);
    }
    match account.launcher {
        None => status("no launcher", Color::DarkGray),
        Some(false) => status("launcher off", Color::Red),
        Some(true) if !account.command_found => status("command missing", Color::Red),
        Some(true) => match account.last_run.as_deref() {
            Some("out of quota") => status("out of quota", Color::Red),
            Some("failed") | Some("timeout") => status("last run failed", Color::Red),
            _ => status("ready", Color::Green),
        },
    }
}

fn project_line(project: &ProjectView) -> Line<'static> {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for task in &project.tasks {
        *counts.entry(task.status.as_str()).or_default() += 1;
    }
    let summary = if counts.is_empty() {
        "no open tasks".to_string()
    } else {
        counts
            .iter()
            .map(|(status, n)| format!("{status} {n}"))
            .collect::<Vec<_>>()
            .join(" · ")
    };
    Line::from(vec![
        Span::styled(project.channel.clone(), bold().fg(Color::Cyan)),
        Span::raw(format!("  {}  ", project.name)),
        Span::styled(
            match &project.pm {
                Some(pm) => format!("PM @{pm}"),
                None => "no PM".to_string(),
            },
            Style::default().fg(Color::Yellow),
        ),
        Span::styled(
            match project.rooms.last() {
                Some(room) => format!("  sprint {}", room.label),
                None => String::new(),
            },
            Style::default().fg(Color::Magenta),
        ),
        Span::styled(format!("   {summary}"), Style::default().fg(Color::Gray)),
    ])
}

fn repo_line(repo: &RepoView) -> Line<'static> {
    let (local, local_color) = if !repo.folder_found {
        ("folder missing".to_string(), Color::Red)
    } else if !repo.local_ok {
        ("not a git repo".to_string(), Color::Red)
    } else {
        (
            format!("local ok ({})", repo.branch.as_deref().unwrap_or("?")),
            Color::Green,
        )
    };
    let remote = match &repo.remote {
        Some(url) => Span::styled(
            format!(
                "remote {}",
                url.trim_start_matches("https://github.com/")
                    .trim_end_matches(".git")
            ),
            Style::default().fg(Color::Cyan),
        ),
        None => Span::styled("no remote", dim()),
    };
    Line::from(vec![
        Span::styled(format!("    repo {}  ", repo.name), dim()),
        Span::styled(local, Style::default().fg(local_color)),
        Span::styled("  on hub  ", Style::default().fg(Color::Green)),
        remote,
    ])
}

/// Home: every project with its repo check, then the hub-wide header.
fn draw_home(frame: &mut Frame, app: &mut App, snap: &Snapshot, area: Rect) {
    let [projects_area, trio] = Layout::vertical([
        Constraint::Min(5),
        Constraint::Length(trio_height(snap, area)),
    ])
    .areas(area);

    app.home_cursor = app.home_cursor.min(snap.projects.len().saturating_sub(1));
    app.home_rows.clear();
    let mut lines: Vec<Line> = Vec::new();
    for (index, project) in snap.projects.iter().enumerate() {
        app.home_rows
            .push((projects_area.y + 1 + lines.len() as u16, index));
        let mut line = project_line(project);
        if index == app.home_cursor {
            line = line.style(Style::default().bg(Color::DarkGray));
        }
        lines.push(line);
        if project.repos.is_empty() {
            lines.push(Line::from(Span::styled("    no repo attached", dim())));
        }
        lines.extend(project.repos.iter().map(repo_line));
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "No projects yet. Press n to create one.",
            dim(),
        )));
    }
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" Projects  (Enter opens a project) ")),
        projects_area,
    );
    draw_trio(frame, snap, trio);
}

fn hint_line(hints: &[(&str, &str)]) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (key, label) in hints {
        spans.push(Span::styled(
            format!("[{key}]"),
            bold().fg(Color::Black).bg(Color::Cyan),
        ));
        spans.push(Span::raw(format!(" {label} ")));
    }
    Line::from(spans)
}

/// The channel list as a tree: a header row per project (or "Hub"), its
/// rooms underneath, numbered, with unread counts on the right.
fn channel_tree(app: &mut App, snap: &Snapshot, area: Rect) -> Vec<Line<'static>> {
    app.channel_rows.clear();
    let width = area.width.saturating_sub(2) as usize;
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut number = 0usize;
    let mut last_group: Option<String> = None;
    for (index, name) in chat_rooms(app, snap).iter().enumerate() {
        let project = project_for(snap, name);
        let title = if name.starts_with('@') {
            "Direct messages".to_string()
        } else {
            project
                .map(|p| p.name.clone())
                .unwrap_or_else(|| "Hub".to_string())
        };
        // A sprint room is listed by its label and title, indented.
        let shown: String = project
            .and_then(|p| p.rooms.iter().find(|r| &r.channel == name))
            .map(|r| format!("  {} {}", r.label, r.title.as_deref().unwrap_or_default()))
            .unwrap_or_else(|| name.clone())
            .chars()
            .take(width.saturating_sub(9))
            .collect();
        if last_group.as_deref() != Some(title.as_str()) {
            number += 1;
            lines.push(Line::from(Span::styled(
                format!("{number:>2} [-] {title:<w$}", w = width.saturating_sub(7)),
                bold().fg(Color::White).bg(Color::Blue),
            )));
            last_group = Some(title);
        }
        number += 1;
        let last_seq = snap
            .messages
            .get(name)
            .and_then(|m| m.last())
            .map(|m| m.seq)
            .unwrap_or(0);
        if index == app.channel_idx {
            app.seen.insert(name.clone(), last_seq);
        }
        let unread = unread(app, snap, name);
        let count = if unread > 0 {
            unread.to_string()
        } else {
            String::new()
        };
        let label = format!("{number:>2}   {shown:<w$}{count:>4}", w = width.saturating_sub(9));
        app.channel_rows
            .push((area.y + 1 + lines.len() as u16, index));
        lines.push(if index == app.channel_idx {
            Line::from(Span::styled(label, bold().fg(Color::Black).bg(Color::Cyan)))
        } else if unread > 0 {
            Line::from(Span::styled(label, bold()))
        } else {
            Line::from(label)
        });
    }
    lines
}

/// What an account is doing in `channel`, from its tasks there: the dot
/// colour and a word. `None` means it is standing by.
pub fn member_state(snap: &Snapshot, channel: &str, nick: &str) -> Option<(Color, &'static str)> {
    // In a project's own room: all its tasks. In a sprint room: that
    // sprint's tasks. Anywhere else: every project.
    let home = project_for(snap, channel);
    let in_sprint_room = home.is_some_and(|p| p.channel != channel);
    let mut best: Option<(u8, Color, &'static str)> = None;
    for project in snap
        .projects
        .iter()
        .filter(|p| home.is_none_or(|h| h.channel == p.channel))
    {
        for task in project.tasks.iter().filter(|t| {
            t.assignee.eq_ignore_ascii_case(nick)
                && (!in_sprint_room || t.sprint.as_deref() == Some(channel))
        }) {
            let escalated = snap
                .waiting
                .iter()
                .any(|w| w.task_id == task.id && w.escalated);
            let state = match task.status.as_str() {
                "blocked" if escalated => (0, Color::Red, "needs you"),
                "blocked" => (1, Color::Yellow, "waiting on PM"),
                "testing" => (2, Color::Magenta, "testing"),
                "review" => (3, Color::Blue, "in review"),
                "in_progress" | "claimed" => (4, Color::Green, "coding"),
                _ => (5, Color::White, "has work"),
            };
            if best.is_none_or(|current| state.0 < current.0) {
                best = Some(state);
            }
        }
    }
    best.map(|(_, color, label)| (color, label)).or_else(|| {
        snap.accounts
            .iter()
            .any(|a| a.nick.eq_ignore_ascii_case(nick) && a.running)
            .then_some((Color::Green, "working"))
    })
}

/// Who is working in this room (above the line) and who is standing by.
/// Clicking a name other than your own opens a direct message.
fn draw_people(frame: &mut Frame, app: &mut App, snap: &Snapshot, channel: &str, area: Rect) {
    let names: Vec<String> = match project_for(snap, channel) {
        Some(project) => project.members.iter().map(|m| m.nick.clone()).collect(),
        None => snap.accounts.iter().map(|a| a.nick.clone()).collect(),
    };
    let mut lines: Vec<Line> = Vec::new();
    let mut standby: Vec<Line> = Vec::new();
    let mut working_names: Vec<String> = Vec::new();
    let mut standby_names: Vec<String> = Vec::new();
    for name in names {
        match member_state(snap, channel, &name) {
            Some((color, label)) => {
                working_names.push(name.clone());
                lines.push(Line::from(vec![
                    Span::styled("● ", Style::default().fg(color)),
                    Span::styled(name, bold()),
                    Span::styled(format!(" {label}"), Style::default().fg(color)),
                ]));
            }
            None => {
                standby_names.push(name.clone());
                standby.push(Line::from(vec![
                    Span::styled("○ ", dim()),
                    Span::styled(name, dim()),
                ]));
            }
        }
    }
    lines.push(Line::from(Span::styled(
        "─".repeat(area.width.saturating_sub(2) as usize),
        dim(),
    )));
    lines.extend(standby);
    app.people_area = area;
    // Rows inside the border: working names, the rule, then standing by.
    let below_rule = working_names.len() + 1;
    app.people_rows = (working_names.into_iter().enumerate())
        .chain(
            standby_names
                .into_iter()
                .enumerate()
                .map(|(at, n)| (at + below_rule, n)),
        )
        .filter(|(_, nick)| !nick.eq_ignore_ascii_case(&snap.me))
        .map(|(at, nick)| (area.y + 1 + at as u16, nick))
        .collect();
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" People (click: DM) ")),
        area,
    );
}

fn draw_dashboard(frame: &mut Frame, app: &mut App, snap: &Snapshot, area: Rect) {
    let [top, bottom] = Layout::vertical([
        Constraint::Length(trio_height(snap, area)),
        Constraint::Min(4),
    ])
    .areas(area);
    draw_trio(frame, snap, top);
    draw_task_list(frame, app, snap, bottom);
}

/// Accounts, what is waiting on you, and recent launches.
fn draw_trio(frame: &mut Frame, snap: &Snapshot, top: Rect) {
    let [accounts_area, right] =
        Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)]).areas(top);
    let [attention_area, runs_area] =
        Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(right);

    // Accounts: working, online or offline, with the projects each manages.
    let accounts: Vec<Line> = snap
        .accounts
        .iter()
        .map(|account| {
            let (marker, color) = if account.running {
                ("> ", Color::Cyan)
            } else if account.online {
                ("* ", Color::Green)
            } else {
                ("  ", Color::DarkGray)
            };
            let (status, status_color) = account_status(account);
            Line::from(vec![
                Span::styled(marker, Style::default().fg(color)),
                Span::styled(format!("{:<12} ", account.nick), bold()),
                Span::raw(format!("{:<12}", account.role)),
                Span::styled(status, Style::default().fg(status_color)),
            ])
        })
        .collect();
    frame.render_widget(
        Paragraph::new(accounts)
            .block(Block::bordered().title(" Accounts  (> working, * online) ")),
        accounts_area,
    );

    // What is waiting on a person.
    let mut attention: Vec<Line> = snap
        .waiting
        .iter()
        .map(|w| {
            let (label, color) = if w.escalated {
                ("NEEDS YOU ", Color::Red)
            } else {
                ("QUESTION  ", Color::Yellow)
            };
            Line::from(vec![
                Span::styled(label, bold().fg(color)),
                Span::raw(format!("{} @{}: {}", w.channel, w.asked_by, w.question)),
            ])
        })
        .collect();
    if attention.is_empty() {
        attention.push(Line::from(Span::styled(
            "Nothing is waiting on you.",
            dim(),
        )));
    }
    frame.render_widget(
        Paragraph::new(attention)
            .wrap(Wrap { trim: false })
            .block(Block::bordered().title(" Waiting on you  (w answer) ")),
        attention_area,
    );

    let runs: Vec<Line> = if snap.runs.is_empty() {
        vec![Line::from(Span::styled(
            "No launches yet (or no owner key on this machine).",
            dim(),
        ))]
    } else {
        snap.runs
            .iter()
            .map(|run| {
                let color = match run.split_whitespace().next() {
                    Some("running") => Color::Cyan,
                    Some("ok") => Color::Green,
                    Some("skipped") => Color::DarkGray,
                    _ => Color::Red,
                };
                Line::from(Span::styled(run.clone(), Style::default().fg(color)))
            })
            .collect()
    };
    frame.render_widget(
        Paragraph::new(runs).block(Block::bordered().title(" Recent launches ")),
        runs_area,
    );

}

/// Projects with their tasks grouped by state: the simple sprint view.
fn draw_task_list(frame: &mut Frame, app: &mut App, snap: &Snapshot, bottom: Rect) {
    let all_rows = rows(snap, app.project.as_deref());
    app.cursor = app.cursor.min(all_rows.len().saturating_sub(1));
    let visible = bottom.height.saturating_sub(2) as usize;
    if app.cursor < app.scroll {
        app.scroll = app.cursor;
    } else if visible > 0 && app.cursor >= app.scroll + visible {
        app.scroll = app.cursor + 1 - visible;
    }
    app.task_rows.clear();
    // How deep each shown task sits under its parents.
    let depths: HashMap<(usize, usize), usize> = snap
        .projects
        .iter()
        .enumerate()
        .filter(|(_, project)| app.project.as_deref().is_none_or(|c| c == project.channel))
        .flat_map(|(p, project)| {
            task_tree(project)
                .into_iter()
                .map(move |(t, d)| ((p, t), d))
        })
        .collect();

    let mut lines: Vec<Line> = Vec::new();
    for (index, row) in all_rows
        .iter()
        .enumerate()
        .skip(app.scroll)
        .take(visible.max(1))
    {
        app.task_rows
            .push((bottom.y + 1 + (index - app.scroll) as u16, index));
        let mut line = match row {
            Row::Project(p) => project_line(&snap.projects[*p]),
            Row::Task(p, t) => {
                let task = &snap.projects[*p].tasks[*t];
                // Subtasks sit indented under their parent. One whose parent
                // is not listed names the parent instead.
                let depth = depths.get(&(*p, *t)).copied().unwrap_or(0);
                let branch = match depth {
                    0 => String::new(),
                    _ => format!("{}└ ", "  ".repeat(depth - 1)),
                };
                let orphan = match task.parent.as_deref() {
                    Some(parent) if depth == 0 => {
                        format!("  sub of {}", parent.chars().take(13).collect::<String>())
                    }
                    _ => String::new(),
                };
                let sprint = task
                    .sprint
                    .as_deref()
                    .and_then(|room| snap.projects[*p].rooms.iter().find(|r| r.channel == room))
                    .map(|r| r.label.clone())
                    .unwrap_or_default();
                Line::from(vec![
                    Span::raw("  "),
                    Span::styled(
                        format!("{:<18}", task.status),
                        Style::default().fg(status_color(&task.status)),
                    ),
                    Span::styled(format!("@{:<14}", task.assignee), bold()),
                    Span::styled(format!("{sprint:<4}"), Style::default().fg(Color::Magenta)),
                    Span::styled(branch, dim()),
                    Span::raw(format!("[{}] {}", task.kind, task.title)),
                    Span::styled(
                        format!("  {}", task.id.chars().take(13).collect::<String>()),
                        dim(),
                    ),
                    Span::styled(orphan, dim()),
                    Span::styled(
                        task.reviewer
                            .as_deref()
                            .map(|r| format!("  review @{r}"))
                            .unwrap_or_default(),
                        dim(),
                    ),
                ])
            }
        };
        if index == app.cursor {
            line = line.style(Style::default().bg(Color::DarkGray));
        }
        lines.push(line);
    }
    if all_rows.is_empty() {
        lines.push(Line::from(Span::styled(
            "No projects yet. Create one with: airc project create <name>",
            dim(),
        )));
    }
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(
            " Projects and tasks  (Enter details · n new · a assign · s status · p PM · m member) ",
        )),
        bottom,
    );
}

fn chat_message<'a>(message: &'a ChatLine, me: &str) -> Text<'a> {
    let name_color = if message.sender == "aircd" {
        Color::DarkGray
    } else if message.sender.eq_ignore_ascii_case(me) {
        Color::Green
    } else {
        Color::Cyan
    };
    let mut body = Text::from(message.body.as_str()).lines.into_iter();
    let mut first = Line::from(vec![
        Span::styled(clock(message.time), dim()),
        Span::styled(format!(" {}: ", message.sender), bold().fg(name_color)),
    ]);
    first.spans.extend(body.next().unwrap_or_default().spans);
    let lines: Vec<Line> = std::iter::once(first).chain(body).collect();
    Text::from(lines)
}

/// Height of the chat message bar: three text rows inside the border.
const CHAT_INPUT_HEIGHT: u16 = 5;

/// Word-wrap chat input to `width` columns. Returns the visual rows and the
/// (column, row) where the cursor sits after the last typed character. The
/// cursor row may be one past the last row when the last row is full.
fn wrap_input(text: &str, width: usize) -> (Vec<String>, (usize, usize)) {
    let width = width.max(1);
    let mut rows = vec![String::new()];
    let mut len = 0;
    for (index, line) in text.split('\n').enumerate() {
        if index > 0 {
            rows.push(String::new());
            len = 0;
        }
        let mut rest = line;
        while !rest.is_empty() {
            let is_space = rest.starts_with(char::is_whitespace);
            let end = rest
                .char_indices()
                .find(|(_, c)| c.is_whitespace() != is_space)
                .map_or(rest.len(), |(at, _)| at);
            let (token, tail) = rest.split_at(end);
            rest = tail;
            let token_len = token.chars().count();
            if is_space {
                for c in token.chars() {
                    if len < width {
                        rows.last_mut().unwrap().push(c);
                        len += 1;
                    } else {
                        // A space that overflows becomes the line break.
                        rows.push(String::new());
                        len = 0;
                    }
                }
                continue;
            }
            if len + token_len > width && len > 0 {
                rows.push(String::new());
                len = 0;
            }
            for c in token.chars() {
                if len == width {
                    rows.push(String::new());
                    len = 0;
                }
                rows.last_mut().unwrap().push(c);
                len += 1;
            }
        }
    }
    let last = rows.len() - 1;
    let cursor = if len >= width {
        (0, last + 1)
    } else {
        (len, last)
    };
    (rows, cursor)
}

/// Draw the chat message bar with wrapped text, keeping the cursor row in view.
fn draw_chat_input(frame: &mut Frame, input: &str, channel: &str, area: Rect) {
    let block = Block::bordered().title(format!(" Message {channel} "));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let (rows, (col, row)) = wrap_input(input, usize::from(inner.width));
    let visible = usize::from(inner.height);
    let top = (row + 1).saturating_sub(visible);
    let lines: Vec<Line> = rows
        .into_iter()
        .skip(top)
        .take(visible)
        .map(Line::from)
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
    frame.set_cursor_position((inner.x + col as u16, inner.y + (row - top) as u16));
}

fn draw_chat(frame: &mut Frame, app: &mut App, snap: &Snapshot, area: Rect) {
    let [channels_area, right, people_area] = Layout::horizontal([
        Constraint::Length(CHANNEL_PANE_WIDTH),
        Constraint::Min(20),
        Constraint::Length(PEOPLE_PANE_WIDTH),
    ])
    .areas(area);
    let [messages_area, input_area] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(CHAT_INPUT_HEIGHT)]).areas(right);

    let rooms = chat_rooms(app, snap);
    // Stay in the same room when rooms are added or reordered.
    if let Some(at) = app
        .chat_channel
        .as_ref()
        .and_then(|room| rooms.iter().position(|r| r == room))
    {
        app.channel_idx = at;
    }
    app.channel_idx = app.channel_idx.min(rooms.len().saturating_sub(1));
    let current = rooms
        .get(app.channel_idx)
        .cloned()
        .unwrap_or_else(|| "#general".to_string());
    if app.chat_channel.as_deref() != Some(&current) {
        app.chat_scroll = 0;
    }
    let channel_lines = channel_tree(app, snap, channels_area);
    frame.render_widget(
        Paragraph::new(channel_lines).block(Block::bordered().title(" Rooms ")),
        channels_area,
    );

    let block = Block::bordered();
    let inner = block.inner(messages_area);
    let messages: Vec<_> = snap
        .messages
        .get(&current)
        .into_iter()
        .flatten()
        .flat_map(|message| chat_message(message, &snap.me).lines)
        .map(|line| {
            let paragraph = Paragraph::new(line).wrap(Wrap { trim: false });
            let rows = paragraph.line_count(inner.width);
            (paragraph, rows)
        })
        .collect();
    let total_rows: usize = messages.iter().map(|(_, rows)| rows).sum();
    // Keep the same history in view when refresh appends messages.
    if app.chat_scroll > 0 && inner.width == app.chat_area.width {
        app.chat_scroll = app
            .chat_scroll
            .saturating_add(total_rows.saturating_sub(app.chat_content_height));
    }
    app.chat_content_height = total_rows;
    app.chat_max_scroll = total_rows.saturating_sub(usize::from(inner.height));
    app.chat_scroll = app.chat_scroll.min(app.chat_max_scroll);
    app.chat_area = inner;
    app.chat_channel = Some(current.clone());
    let title = if app.chat_scroll == 0 {
        format!(" {current}  (times UTC) ")
    } else {
        format!(" {current}  [{} lines up] ", app.chat_scroll)
    };
    frame.render_widget(block.title(title), messages_area);

    let mut skipped = app.chat_max_scroll.saturating_sub(app.chat_scroll);
    let mut remaining = inner;
    // Render each source line separately so a long history can exceed the
    // paragraph widget's u16 scroll range.
    for (paragraph, rows) in messages {
        if skipped >= rows {
            skipped -= rows;
            continue;
        }
        let height = (rows - skipped).min(usize::from(remaining.height)) as u16;
        frame.render_widget(
            paragraph.scroll((skipped as u16, 0)),
            Rect {
                height,
                ..remaining
            },
        );
        remaining.y += height;
        remaining.height -= height;
        skipped = 0;
        if remaining.height == 0 {
            break;
        }
    }
    if total_rows == 0 {
        frame.render_widget(
            Paragraph::new(Span::styled("No messages here yet.", dim())),
            inner,
        );
    }

    draw_chat_input(frame, &app.input, &current, input_area);
    draw_people(frame, app, snap, &current, people_area);
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width.saturating_sub(2)).max(10);
    let height = height.min(area.height.saturating_sub(1)).max(3);
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}

fn draw_modal(frame: &mut Frame, modal: &Modal, snap: &Snapshot, area: Rect) {
    let frame_style = Style::default().fg(Color::Cyan);
    match modal {
        Modal::Picker {
            title, items, idx, ..
        } => {
            let widest = items
                .iter()
                .map(|(label, _)| label.chars().count())
                .max()
                .unwrap_or(10)
                .max(title.chars().count());
            let rect = centered(area, widest as u16 + 8, items.len() as u16 + 2);
            let visible = rect.height.saturating_sub(2) as usize;
            let first = idx.saturating_sub(visible.saturating_sub(1));
            let lines: Vec<Line> = items
                .iter()
                .enumerate()
                .skip(first)
                .take(visible)
                .map(|(i, (label, _))| {
                    if i == *idx {
                        Line::from(Span::styled(
                            format!("> {label}"),
                            bold().fg(Color::Black).bg(Color::Cyan),
                        ))
                    } else {
                        Line::from(format!("  {label}"))
                    }
                })
                .collect();
            frame.render_widget(Clear, rect);
            frame.render_widget(
                Paragraph::new(lines).block(
                    Block::bordered()
                        .border_style(frame_style)
                        .title(format!(" {title} ")),
                ),
                rect,
            );
        }
        Modal::Prompt { title, text, .. } => {
            let rect = centered(area, 78, 3);
            frame.render_widget(Clear, rect);
            frame.render_widget(
                Paragraph::new(text.clone()).block(
                    Block::bordered()
                        .border_style(frame_style)
                        .title(format!(" {title} ")),
                ),
                rect,
            );
            let cursor_x =
                rect.x + 1 + (text.chars().count() as u16).min(rect.width.saturating_sub(3));
            frame.set_cursor_position((cursor_x, rect.y + 1));
        }
        Modal::Models { template, idx, .. } => {
            let (mut lines, problem): (Vec<Line>, Option<String>) = match model_items(snap, template) {
                None => (vec![Line::from("  Asking the hub for the models...")], None),
                Some((items, problem)) => (
                    items
                        .iter()
                        .enumerate()
                        .map(|(i, (label, _))| {
                            if i == *idx {
                                Line::from(Span::styled(
                                    format!("> {label}"),
                                    bold().fg(Color::Black).bg(Color::Cyan),
                                ))
                            } else {
                                Line::from(format!("  {label}"))
                            }
                        })
                        .collect(),
                    problem,
                ),
            };
            let mut skip_from = *idx;
            if let Some(problem) = problem {
                lines.insert(
                    0,
                    Line::from(Span::styled(format!("  {problem}"), Style::default().fg(Color::Yellow))),
                );
                skip_from += 1;
            }
            let widest = lines.iter().map(|l| l.width()).max().unwrap_or(30).max(40);
            let rect = centered(area, widest as u16 + 6, lines.len() as u16 + 2);
            let visible = rect.height.saturating_sub(2) as usize;
            let first = skip_from.saturating_sub(visible.saturating_sub(1));
            let shown: Vec<Line> = lines.into_iter().skip(first).take(visible).collect();
            frame.render_widget(Clear, rect);
            frame.render_widget(
                Paragraph::new(shown).block(
                    Block::bordered()
                        .border_style(frame_style)
                        .title(format!(" Model for {template} ")),
                ),
                rect,
            );
        }
        Modal::Detail { task_id } => {
            let lines: Vec<Line> = match &snap.detail {
                Some((id, lines)) if id == task_id => {
                    lines.iter().map(|l| Line::from(l.clone())).collect()
                }
                _ => vec![Line::from("Loading...")],
            };
            let rect = centered(area, 96, lines.len() as u16 + 2);
            frame.render_widget(Clear, rect);
            frame.render_widget(
                Paragraph::new(lines).wrap(Wrap { trim: false }).block(
                    Block::bordered()
                        .border_style(frame_style)
                        .title(format!(" Task {task_id}  (Esc close) ")),
                ),
                rect,
            );
        }
        Modal::Help => {
            let rect = centered(area, 78, HELP.len() as u16 + 2);
            frame.render_widget(Clear, rect);
            frame.render_widget(
                Paragraph::new(HELP.iter().map(|l| Line::from(*l)).collect::<Vec<_>>()).block(
                    Block::bordered()
                        .border_style(frame_style)
                        .title(" Keys  (Esc close) "),
                ),
                rect,
            );
        }
    }
}

// ---------------------------------------------------------------- run

fn ui_loop(shared: Arc<Mutex<Snapshot>>, commands: UnboundedSender<Action>) -> Result<()> {
    let mut terminal = ratatui::init();
    let _ = execute!(std::io::stdout(), EnableMouseCapture);
    let mut app = App {
        view: View::Home,
        ..App::default()
    };

    let outcome = (|| -> Result<()> {
        loop {
            let snap = shared.lock().expect("snapshot mutex poisoned").clone();
            terminal.draw(|frame| draw(frame, &mut app, &snap))?;
            if !event::poll(Duration::from_millis(150))? {
                continue;
            }
            let action = match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    handle_key(&mut app, key.code, key.modifiers, &snap)
                }
                Event::Mouse(mouse) => handle_mouse(&mut app, mouse),
                _ => None,
            };
            match action {
                Some(Action::Quit) => return Ok(()),
                Some(other) => {
                    let _ = commands.send(other);
                }
                None => {}
            }
        }
    })();

    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    outcome
}

/// Draw one frame off-screen and return it as plain text, one row per line.
pub fn render_to_text(app: &mut App, snap: &Snapshot, width: u16, height: u16) -> Result<String> {
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))?;
    terminal.draw(|frame| draw(frame, app, snap))?;
    let buffer = terminal.backend().buffer();
    let cells: Vec<&str> = buffer.content().iter().map(|cell| cell.symbol()).collect();
    Ok(cells
        .chunks(width as usize)
        .map(|row| row.concat().trim_end().to_string())
        .collect::<Vec<_>>()
        .join("\n"))
}

/// Entry point for `aircd tui [--server URL] [--profile NAME]`.
pub async fn run(args: Vec<String>) -> Result<()> {
    let mut server = None;
    let mut profile = None;
    let mut dump = false;
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--server" => server = Some(iter.next().context("--server requires a URL")?),
            "--profile" => profile = Some(iter.next().context("--profile requires a name")?),
            "--dump" => dump = true,
            "-h" | "--help" => {
                println!(
                    "aircd tui - dashboard, chat and control panel for a running hub\n\
                     Flags:\n  \
                       --server <url>     Hub address (default: the profile's, or http://127.0.0.1:6680)\n  \
                       --profile <name>   Saved airc profile to sign in as (default: your default profile)\n  \
                       --dump             Print the dashboard and chat screens once as plain text and exit\n\
                     Press ? on the dashboard for the list of keys."
                );
                return Ok(());
            }
            other => return Err(anyhow!("unknown argument for tui: {other} (use --help)")),
        }
    }
    let cfg = load_config(server, profile)?;

    let shared = Arc::new(Mutex::new(Snapshot::default()));
    // Fail with a clear message before taking over the terminal.
    refresh(&reqwest::Client::new(), &cfg, &shared).await?;

    if dump {
        let snap = shared.lock().expect("snapshot mutex poisoned").clone();
        let last_channel = snap.channels.len().saturating_sub(1);
        for mut app in [
            App {
                view: View::Home,
                ..App::default()
            },
            App::default(),
            App {
                view: View::Chat,
                channel_idx: last_channel,
                ..App::default()
            },
        ] {
            println!("{}", render_to_text(&mut app, &snap, 110, 30)?);
        }
        return Ok(());
    }

    let (tx, rx) = unbounded_channel();
    let stop = Arc::new(AtomicBool::new(false));
    let fetcher = tokio::spawn(fetch_loop(cfg, shared.clone(), rx, stop.clone()));
    let outcome = tokio::task::spawn_blocking(move || ui_loop(shared, tx)).await;
    stop.store(true, Ordering::Relaxed);
    fetcher.abort();
    outcome.context("interface task failed")?
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: KeyModifiers = KeyModifiers::NONE;

    fn fixture() -> Snapshot {
        let mut messages = BTreeMap::new();
        messages.insert(
            "#quest-video".to_string(),
            vec![ChatLine {
                seq: 1,
                sender: "flash3.8".to_string(),
                body: "Stepping in as PM".to_string(),
                time: 3_600 * 14 + 60 * 5,
            }],
        );
        let account = |nick: &str, role: &str, online: bool, running: bool| Account {
            nick: nick.into(),
            role: role.into(),
            online,
            running,
            ..Account::default()
        };
        Snapshot {
            server: "http://127.0.0.1:6680".to_string(),
            me: "tony".to_string(),
            my_role: "leader".to_string(),
            accounts: vec![
                account("tony", "leader", true, false),
                Account {
                    launcher: Some(true),
                    command_found: true,
                    last_run: Some("ok".into()),
                    ..account("flash3.8", "builder", false, true)
                },
                Account {
                    launcher: Some(false),
                    command_found: true,
                    ..account("flash-lite", "builder", false, false)
                },
            ],
            projects: vec![ProjectView {
                channel: "#quest-video".into(),
                name: "Quest player".into(),
                pm: Some("flash3.8".into()),
                tasks: vec![TaskView {
                    id: "task_abc123".into(),
                    kind: "bug".into(),
                    status: "blocked".into(),
                    assignee: "codex".into(),
                    title: "Fix seek bar".into(),
                    sprint: Some("#quest-video-s1".into()),
                    ..TaskView::default()
                }],
                members: vec![
                    Member {
                        nick: "flash3.8".into(),
                        role: "coordinator".into(),
                    },
                    Member {
                        nick: "codex".into(),
                        role: "builder".into(),
                    },
                ],
                repos: vec![RepoView {
                    name: "quest-player".into(),
                    folder_found: false,
                    local_ok: false,
                    branch: None,
                    remote: Some("https://github.com/antv311/quest-video-workspace.git".into()),
                }],
                rooms: vec![RoomView {
                    channel: "#quest-video-s1".into(),
                    label: "s1".into(),
                    title: Some("Local playback".into()),
                }],
            }],
            waiting: vec![Waiting {
                task_id: "task_abc123".into(),
                channel: "#quest-video".into(),
                asked_by: "codex".into(),
                question: "May I buy a licence?".into(),
                escalated: true,
            }],
            launchers: vec![Launcher {
                nick: "flash-lite".into(),
                enabled: false,
                mode: "fresh".into(),
            }],
            runs: vec!["ok       @flash3.8  role in #quest-video".into()],
            channels: vec![
                "#general".into(),
                "#quest-video".into(),
                "#quest-video-s1".into(),
            ],
            messages,
            dm_seq: 0,
            detail: None,
            templates: Vec::new(),
            models: None,
            notice: None,
            error: None,
        }
    }

    fn render(app: &mut App, snap: &Snapshot) -> String {
        render_to_text(app, snap, 110, 32).unwrap()
    }

    fn press(app: &mut App, snap: &Snapshot, keys: &[KeyCode]) -> Option<Action> {
        let mut last = None;
        for key in keys {
            last = handle_key(app, *key, NONE, snap);
        }
        last
    }

    fn type_text(app: &mut App, snap: &Snapshot, text: &str) {
        for c in text.chars() {
            handle_key(app, KeyCode::Char(c), NONE, snap);
        }
    }

    fn sent(path: &str, body: Value, done: &str) -> Option<Action> {
        Some(Action::Api {
            path: path.into(),
            body,
            owner: false,
            done: done.into(),
        })
    }

    #[test]
    fn dashboard_shows_accounts_projects_tasks_and_what_needs_the_owner() {
        let screen = render(&mut App::default(), &fixture());
        for expected in [
            "[D]ashboard",
            "you: @tony (leader)",
            "flash3.8",
            "working",
            "no launcher",
            "launcher off",
            "NEEDS YOU",
            "May I buy a licence?",
            "#quest-video",
            "PM @flash3.8",
            "blocked 1",
            "Fix seek bar",
            "Recent launches",
        ] {
            assert!(screen.contains(expected), "dashboard is missing {expected:?}");
        }
    }

    #[test]
    fn chat_shows_the_selected_channel_and_typed_text() {
        let snap = fixture();
        let mut app = App {
            view: View::Chat,
            channel_idx: 1,
            ..App::default()
        };
        app.input = "hello team".to_string();
        let screen = render(&mut app, &snap);
        for expected in [
            "[-] Quest player",
            "#quest-video",
            "codex needs you",
            "flash3.8 working",
            "14:05",
            "flash3.8: ",
            "Stepping in as PM",
            "hello team",
        ] {
            assert!(screen.contains(expected), "chat is missing {expected:?}");
        }
    }

    #[test]
    fn wrap_input_breaks_at_words_and_tracks_the_cursor() {
        assert_eq!(wrap_input("", 10), (vec![String::new()], (0, 0)));
        assert_eq!(wrap_input("hello", 10), (vec!["hello".to_string()], (5, 0)));
        let (rows, cursor) = wrap_input("hello wide world", 10);
        assert_eq!(rows, ["hello wide", "world"]);
        assert_eq!(cursor, (5, 1));
        let (rows, cursor) = wrap_input("hello abcdefghij", 10);
        assert_eq!(rows, ["hello ", "abcdefghij"]);
        assert_eq!(cursor, (0, 2), "a full last row moves the cursor down");
        let (rows, cursor) = wrap_input("hello world", 10);
        assert_eq!(rows, ["hello ", "world"]);
        assert_eq!(cursor, (5, 1));
        let (rows, cursor) = wrap_input("abcdefghij ", 10);
        assert_eq!(
            rows,
            ["abcdefghij", ""],
            "an overflowing space breaks the line"
        );
        assert_eq!(cursor, (0, 1));
        let (rows, cursor) = wrap_input("hi abcdefghijklmn", 10);
        assert_eq!(rows, ["hi ", "abcdefghij", "klmn"], "long words split");
        assert_eq!(cursor, (4, 2));
    }

    fn render_chat_input(input: &str, width: u16) -> (String, (u16, u16), Rect) {
        let snap = fixture();
        let mut app = App {
            view: View::Chat,
            channel_idx: 1,
            ..App::default()
        };
        app.input = input.to_string();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 32)).unwrap();
        let mut input_area = Rect::default();
        terminal.draw(|frame| draw(frame, &mut app, &snap)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let lines: Vec<String> = buffer
            .content()
            .chunks(usize::from(width))
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect();
        if let Some(top) = lines
            .iter()
            .position(|line| line.contains(" Message #quest-video "))
        {
            let left = lines[top].chars().take_while(|c| *c != '┌').count() as u16;
            input_area = Rect::new(left, top as u16, 0, CHAT_INPUT_HEIGHT);
        }
        let cursor = terminal.get_cursor_position().unwrap();
        (lines.join("\n"), (cursor.x, cursor.y), input_area)
    }

    #[test]
    fn chat_input_wraps_long_messages_and_the_cursor_follows() {
        let words: Vec<String> = (0..12).map(|i| format!("word{i:02}")).collect();
        let text = words.join(" ");
        let (screen, cursor, bar) = render_chat_input(&text, 110);
        assert_eq!(CHAT_INPUT_HEIGHT, 5);
        let rows: Vec<&str> = screen.lines().collect();
        let inside: Vec<String> = (1..4)
            .map(|offset| rows[usize::from(bar.y + offset)].to_string())
            .collect();
        for word in &words {
            assert!(
                inside.iter().any(|row| row.contains(word.as_str())),
                "message bar is missing {word:?}: {inside:#?}"
            );
        }
        assert!(
            inside.iter().filter(|row| row.contains("word")).count() >= 2,
            "long input should span several rows: {inside:#?}"
        );
        assert!(
            cursor.1 > bar.y + 1,
            "cursor should follow the text down: {cursor:?}"
        );
        let cursor_row = rows[usize::from(cursor.1)];
        let before: String = cursor_row.chars().take(usize::from(cursor.0)).collect();
        assert!(
            before.trim_end().ends_with("word11"),
            "cursor row: {cursor_row:?}"
        );
    }

    #[test]
    fn chat_input_scrolls_to_keep_the_cursor_visible() {
        let text = (0..60)
            .map(|i| format!("w{i:02}"))
            .collect::<Vec<_>>()
            .join(" ");
        let (screen, cursor, bar) = render_chat_input(&text, 110);
        assert_eq!(cursor.1, bar.y + 3, "cursor sits on the last text row");
        let rows: Vec<&str> = screen.lines().collect();
        assert!(rows[usize::from(cursor.1)].contains("w59"));
        assert!(
            !screen.contains("w00 "),
            "oldest text scrolls out of the bar"
        );
    }

    fn chat_with_history(count: usize) -> (App, Snapshot) {
        let mut snap = fixture();
        snap.messages.insert(
            "#quest-video".into(),
            (0..count)
                .map(|index| ChatLine {
                    seq: index as i64 + 1,
                    sender: "codex".into(),
                    body: format!("message-{index:03}"),
                    time: 0,
                })
                .collect(),
        );
        let app = App {
            view: View::Chat,
            channel_idx: 1,
            ..App::default()
        };
        (app, snap)
    }

    #[test]
    fn chat_page_keys_show_history_and_clamp_at_both_ends() {
        let (mut app, snap) = chat_with_history(80);
        let latest = render(&mut app, &snap);
        assert!(latest.contains("message-079"));
        assert!(!latest.contains("message-000"));
        assert_eq!(app.chat_scroll, 0);
        let page = usize::from(app.chat_area.height);
        assert_eq!(app.chat_max_scroll, 80 - page);

        press(&mut app, &snap, &[KeyCode::PageUp]);
        assert_eq!(app.chat_scroll, page);
        let history = render(&mut app, &snap);
        assert!(!history.contains("message-079"));
        assert!(history.contains(&format!("[{page} lines up]")));

        press(&mut app, &snap, &[KeyCode::Home, KeyCode::PageUp]);
        assert_eq!(app.chat_scroll, app.chat_max_scroll);
        assert!(render(&mut app, &snap).contains("message-000"));
        press(&mut app, &snap, &[KeyCode::PageDown]);
        assert_eq!(app.chat_scroll, app.chat_max_scroll - page);
        press(&mut app, &snap, &[KeyCode::End, KeyCode::PageDown]);
        assert_eq!(app.chat_scroll, 0);
        assert!(render(&mut app, &snap).contains("message-079"));
    }

    #[test]
    fn chat_refresh_keeps_history_in_place_and_follows_the_latest_at_bottom() {
        let (mut app, mut snap) = chat_with_history(80);
        render(&mut app, &snap);
        press(&mut app, &snap, &[KeyCode::PageUp]);
        let before = render(&mut app, &snap);
        let offset = app.chat_scroll;
        snap.messages
            .get_mut("#quest-video")
            .unwrap()
            .push(ChatLine {
                seq: 81,
                sender: "codex".into(),
                body: "newest arrival".into(),
                time: 0,
            });
        let after = render(&mut app, &snap);
        assert_eq!(app.chat_scroll, offset + 1);
        let pane = |screen: &str| {
            screen
                .lines()
                .skip(usize::from(app.chat_area.y))
                .take(usize::from(app.chat_area.height))
                .map(|line| {
                    line.chars()
                        .skip(usize::from(app.chat_area.x))
                        .take(usize::from(app.chat_area.width))
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(pane(&before), pane(&after));
        render(&mut app, &snap);
        assert_eq!(app.chat_scroll, offset + 1);

        press(&mut app, &snap, &[KeyCode::End]);
        assert!(render(&mut app, &snap).contains("newest arrival"));
        snap.messages
            .get_mut("#quest-video")
            .unwrap()
            .push(ChatLine {
                seq: 82,
                sender: "codex".into(),
                body: "next arrival".into(),
                time: 0,
            });
        assert!(render(&mut app, &snap).contains("next arrival"));
        assert_eq!(app.chat_scroll, 0);
    }

    #[test]
    fn chat_input_edits_completion_and_send_snap_to_latest() {
        let (mut app, snap) = chat_with_history(80);
        render(&mut app, &snap);
        for key in [KeyCode::Char('x'), KeyCode::Backspace, KeyCode::Tab] {
            app.input = "hi @fl".into();
            app.chat_scroll = 5;
            press(&mut app, &snap, &[key]);
            assert_eq!(app.chat_scroll, 0, "input key {key:?}");
        }
        app.input = "hello".into();
        app.chat_scroll = 5;
        assert_eq!(
            press(&mut app, &snap, &[KeyCode::Enter]),
            Some(Action::Send {
                target: "#quest-video".into(),
                body: "hello".into()
            })
        );
        assert_eq!(app.chat_scroll, 0);
        assert!(app.input.is_empty());
        app.chat_scroll = 5;
        assert_eq!(press(&mut app, &snap, &[KeyCode::Enter]), None);
        assert_eq!(app.chat_scroll, 0);
    }

    #[test]
    fn chat_room_navigation_and_clicks_reset_history() {
        let (mut app, snap) = chat_with_history(80);
        render(&mut app, &snap);
        for key in [KeyCode::Up, KeyCode::Down] {
            app.chat_scroll = 5;
            press(&mut app, &snap, &[key]);
            assert_eq!(app.chat_scroll, 0);
        }
        app.chat_scroll = 5;
        let (row, _) = app.channel_rows[0];
        handle_click(&mut app, 3, row);
        assert_eq!(app.channel_idx, 0);
        assert_eq!(app.chat_scroll, 0);
        assert!(render(&mut app, &snap).contains("No messages here yet."));
        press(&mut app, &snap, &[KeyCode::Home, KeyCode::PageUp]);
        assert_eq!(app.chat_scroll, 0);
    }

    #[test]
    fn chat_mouse_wheel_scrolls_only_over_the_message_pane() {
        let (mut app, snap) = chat_with_history(80);
        render(&mut app, &snap);
        let mouse = MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: app.chat_area.x,
            row: app.chat_area.y,
            modifiers: NONE,
        };
        assert_eq!(handle_mouse(&mut app, mouse), None);
        assert_eq!(app.chat_scroll, 3);
        handle_mouse(&mut app, MouseEvent { column: 0, ..mouse });
        assert_eq!(app.chat_scroll, 3);
        app.modal = Some(Modal::Help);
        handle_mouse(&mut app, mouse);
        assert_eq!(app.chat_scroll, 3);
        app.modal = None;
        app.view = View::Dashboard;
        handle_mouse(&mut app, mouse);
        assert_eq!(app.chat_scroll, 3);
        app.view = View::Chat;
        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                ..mouse
            },
        );
        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                ..mouse
            },
        );
        assert_eq!(app.chat_scroll, 0);
        app.chat_scroll = app.chat_max_scroll;
        handle_mouse(&mut app, mouse);
        assert_eq!(app.chat_scroll, app.chat_max_scroll);
    }

    #[test]
    fn chat_scrolls_within_wrapped_unicode_and_multiline_messages() {
        let (mut app, mut snap) = chat_with_history(1);
        snap.messages.get_mut("#quest-video").unwrap()[0].body = format!(
            "FIRST\n{}\nLAST",
            "界界界 alpha beta gamma delta epsilon ".repeat(100)
        );
        let latest = render(&mut app, &snap);
        assert!(latest.contains("LAST"));
        assert!(!latest.contains("FIRST"));
        assert!(app.chat_max_scroll > 0);
        press(&mut app, &snap, &[KeyCode::Home]);
        let oldest = render(&mut app, &snap);
        assert!(oldest.contains("FIRST"));
        assert!(!oldest.contains("LAST"));
        press(&mut app, &snap, &[KeyCode::PageDown]);
        let middle = render(&mut app, &snap);
        assert!(!middle.contains("FIRST"));
        assert!(!middle.contains("LAST"));
        // The text dump includes the continuation cell of each wide glyph.
        assert!(middle.contains("界 界 界"), "{middle}");
    }

    #[test]
    fn chat_history_can_exceed_the_paragraph_scroll_range() {
        let (mut app, mut snap) = chat_with_history(1);
        snap.messages.get_mut("#quest-video").unwrap()[0].body = (0..66_000)
            .map(|index| format!("row-{index:05}"))
            .collect::<Vec<_>>()
            .join("\n");
        let latest = render(&mut app, &snap);
        assert!(latest.contains("row-65999"));
        assert!(app.chat_max_scroll > usize::from(u16::MAX));
        press(&mut app, &snap, &[KeyCode::Home]);
        assert!(render(&mut app, &snap).contains("row-00000"));
        press(&mut app, &snap, &[KeyCode::End]);
        assert!(render(&mut app, &snap).contains("row-65999"));
    }

    #[test]
    fn chat_resizing_and_shortened_history_clamp_scroll_without_panicking() {
        let (mut app, mut snap) = chat_with_history(40);
        render(&mut app, &snap);
        press(&mut app, &snap, &[KeyCode::Home]);
        assert!(app.chat_scroll > 0);
        render_to_text(&mut app, &snap, 180, 80).unwrap();
        assert_eq!(app.chat_scroll, 0);
        for (width, height) in [(110, 32), (60, 10), (20, 6), (1, 1)] {
            render_to_text(&mut app, &snap, width, height).unwrap();
            assert!(app.chat_scroll <= app.chat_max_scroll);
        }
        render(&mut app, &snap);
        press(&mut app, &snap, &[KeyCode::Home]);
        snap.messages.get_mut("#quest-video").unwrap().truncate(1);
        assert!(render(&mut app, &snap).contains("message-000"));
        assert_eq!(app.chat_scroll, 0);
    }

    #[test]
    fn keys_switch_views_send_messages_and_complete_names() {
        let snap = fixture();
        let mut app = App::default();

        assert_eq!(press(&mut app, &snap, &[KeyCode::Char('c')]), None);
        assert_eq!(app.view, View::Chat);
        // In chat, letters are text: q must not quit.
        assert_eq!(press(&mut app, &snap, &[KeyCode::Char('q')]), None);
        assert_eq!(app.input, "q");
        app.input.clear();

        press(&mut app, &snap, &[KeyCode::Down]);
        type_text(&mut app, &snap, "ping @flash3");
        press(&mut app, &snap, &[KeyCode::Tab]);
        assert_eq!(app.input, "ping @flash3.8 ");
        assert_eq!(
            press(&mut app, &snap, &[KeyCode::Enter]),
            Some(Action::Send {
                target: "#quest-video".into(),
                body: "ping @flash3.8".into()
            })
        );
        assert!(app.input.is_empty());

        assert_eq!(press(&mut app, &snap, &[KeyCode::Esc]), None);
        assert_eq!(app.view, View::Dashboard);
        assert_eq!(
            press(&mut app, &snap, &[KeyCode::Char('q')]),
            Some(Action::Quit)
        );
        assert_eq!(
            handle_key(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL, &snap),
            Some(Action::Quit)
        );
    }

    #[test]
    fn completion_extends_to_the_shared_prefix_when_several_names_match() {
        let accounts = fixture().accounts;
        assert_eq!(
            complete_mention("hi @fl", &accounts).as_deref(),
            Some("hi @flash")
        );
        assert_eq!(
            complete_mention("hi @to", &accounts).as_deref(),
            Some("hi @tony ")
        );
        assert_eq!(complete_mention("hi @zz", &accounts), None);
        assert_eq!(complete_mention("no mention", &accounts), None);
    }

    #[test]
    fn clicking_the_tabs_a_channel_row_and_a_task_row_works() {
        let snap = fixture();
        let mut app = App::default();
        render(&mut app, &snap); // records where the task rows are
        let (task_row, _) = app.task_rows[1];
        handle_click(&mut app, 10, task_row);
        assert_eq!(app.cursor, 1);

        let chat_x = TABS.find("[C]hat").unwrap() as u16 + 1;
        assert_eq!(handle_click(&mut app, chat_x, 0), None);
        assert_eq!(app.view, View::Chat);
        render(&mut app, &snap); // records where the channel rows are
        let (row, _) = app.channel_rows[1];
        handle_click(&mut app, 3, row);
        assert_eq!(app.channel_idx, 1);
        let quit_x = TABS.find("[Q]uit").unwrap() as u16 + 1;
        assert_eq!(handle_click(&mut app, quit_x, 0), Some(Action::Quit));
    }

    #[test]
    fn new_task_flow_collects_title_assignee_kind_and_description() {
        let snap = fixture();
        let mut app = App::default();
        press(&mut app, &snap, &[KeyCode::Char('n')]);
        assert!(render(&mut app, &snap).contains("New task in #quest-video: title"));
        type_text(&mut app, &snap, "Add subtitles");
        press(&mut app, &snap, &[KeyCode::Enter]);
        assert!(render(&mut app, &snap).contains("@codex  (builder)"));
        // Second member (codex), then the second kind (bug), then a description.
        press(
            &mut app,
            &snap,
            &[KeyCode::Down, KeyCode::Enter, KeyCode::Down, KeyCode::Enter],
        );
        type_text(&mut app, &snap, "SRT files only");
        assert_eq!(
            press(&mut app, &snap, &[KeyCode::Enter]),
            sent(
                "/v1/tasks",
                json!({
                    "channel": "#quest-video", "title": "Add subtitles", "kind": "bug",
                    "assignee": "codex", "body": "SRT files only",
                }),
                "Task created"
            )
        );
        assert!(app.modal.is_none());

        // Esc at any step cancels without sending anything.
        press(&mut app, &snap, &[KeyCode::Char('n')]);
        type_text(&mut app, &snap, "Oops");
        assert_eq!(press(&mut app, &snap, &[KeyCode::Enter, KeyCode::Esc]), None);
        assert!(app.modal.is_none());
    }

    #[test]
    fn task_actions_need_a_selected_task_and_build_the_right_requests() {
        let snap = fixture();
        let mut app = App::default();
        // On the project row there is no task to act on.
        press(&mut app, &snap, &[KeyCode::Char('a')]);
        assert!(app.modal.is_none());

        press(&mut app, &snap, &[KeyCode::Down]);
        assert_eq!(
            press(&mut app, &snap, &[KeyCode::Char('a'), KeyCode::Enter]),
            sent(
                "/v1/tasks/task_abc123/assign",
                json!({"assignee": "flash3.8"}),
                "Task reassigned"
            )
        );
        // The last choice returns the task to the pool.
        assert_eq!(
            press(
                &mut app,
                &snap,
                &[KeyCode::Char('a'), KeyCode::Down, KeyCode::Down, KeyCode::Enter]
            ),
            sent(
                "/v1/tasks/task_abc123/assign",
                json!({"assignee": null}),
                "Task reassigned"
            )
        );
        // Status: the third choice, with a note.
        press(
            &mut app,
            &snap,
            &[
                KeyCode::Char('s'),
                KeyCode::Down,
                KeyCode::Down,
                KeyCode::Down,
                KeyCode::Enter,
            ],
        );
        type_text(&mut app, &snap, "needs a test");
        assert_eq!(
            press(&mut app, &snap, &[KeyCode::Enter]),
            sent(
                "/v1/tasks/task_abc123/status",
                json!({"status": "changes_requested", "note": "needs a test"}),
                "Task status changed"
            )
        );
        // Enter opens the task and asks for its details.
        assert_eq!(
            press(&mut app, &snap, &[KeyCode::Enter]),
            Some(Action::LoadTask("task_abc123".into()))
        );
        assert!(render(&mut app, &snap).contains("Loading..."));
        press(&mut app, &snap, &[KeyCode::Esc]);
        assert!(app.modal.is_none());
    }

    #[test]
    fn project_owner_and_question_actions_build_the_right_requests() {
        let snap = fixture();
        let mut app = App::default();
        // PM: the second account in the list.
        assert_eq!(
            press(
                &mut app,
                &snap,
                &[KeyCode::Char('p'), KeyCode::Down, KeyCode::Enter]
            ),
            sent(
                "/v1/projects/pm",
                json!({"channel": "#quest-video", "nick": "flash3.8"}),
                "Project manager set"
            )
        );
        // Members: every account, members first with their role, then the
        // others, then a new account.
        press(&mut app, &snap, &[KeyCode::Char('m')]);
        let screen = render(&mut app, &snap);
        assert!(screen.contains("Members of #quest-video"), "{screen}");
        assert!(screen.contains("@flash3.8  (coordinator, change role)"), "{screen}");
        assert!(screen.contains("@flash-lite  (add)"), "{screen}");
        assert!(screen.contains("+ new account (client and model)"), "{screen}");
        assert!(
            screen.find("@flash3.8  (coordinator").unwrap() < screen.find("@tony  (add)").unwrap()
                && screen.find("@flash-lite  (add)").unwrap() < screen.find("+ new account").unwrap(),
            "{screen}"
        );
        assert_eq!(
            press(
                &mut app,
                &snap,
                &[KeyCode::Down, KeyCode::Down, KeyCode::Enter, KeyCode::Down, KeyCode::Enter]
            ),
            sent(
                "/v1/projects/members",
                json!({"channel": "#quest-video", "nick": "flash-lite", "role": "builder"}),
                "@flash-lite is now builder in #quest-video"
            )
        );
        // Answer the waiting question.
        press(&mut app, &snap, &[KeyCode::Char('w'), KeyCode::Enter]);
        assert!(render(&mut app, &snap).contains("Answer: May I buy a licence?"));
        type_text(&mut app, &snap, "Yes");
        assert_eq!(
            press(&mut app, &snap, &[KeyCode::Enter]),
            sent(
                "/v1/tasks/task_abc123/answer",
                json!({"answer": "Yes"}),
                "Answer sent"
            )
        );
        // The launcher switch uses the owner key.
        assert_eq!(
            press(&mut app, &snap, &[KeyCode::Char('l'), KeyCode::Enter]),
            Some(Action::Api {
                path: "/v1/owner/wake".into(),
                body: json!({"action": "enable", "nick": "flash-lite"}),
                owner: true,
                done: "Launcher for @flash-lite: enabled".into(),
            })
        );
        // Help opens and closes.
        press(&mut app, &snap, &[KeyCode::Char('?')]);
        assert!(render(&mut app, &snap).contains("switch an account's launcher on or off"));
        press(&mut app, &snap, &[KeyCode::Esc]);
        assert!(app.modal.is_none());
    }

    #[test]
    fn a_recent_action_result_replaces_the_hint_line() {
        let mut snap = fixture();
        snap.notice = Some((true, "Task created".into(), Instant::now()));
        assert!(render(&mut App::default(), &snap).contains("Task created"));
        snap.notice = Some((false, "only the owner can choose".into(), Instant::now()));
        assert!(render(&mut App::default(), &snap).contains("only the owner can choose"));
    }

    fn home() -> App {
        App {
            view: View::Home,
            ..App::default()
        }
    }

    #[test]
    fn home_lists_projects_with_repo_and_agent_checks_and_opens_a_project() {
        let snap = fixture();
        let mut app = home();
        let screen = render(&mut app, &snap);
        for expected in [
            "[H]ome",
            "#quest-video",
            "PM @flash3.8",
            "repo quest-player",
            "folder missing",
            "on hub",
            "remote antv311/quest-video-workspace",
            "working",
            "launcher off",
            "no launcher",
            "[n] new project",
        ] {
            assert!(screen.contains(expected), "home is missing {expected:?}");
        }

        assert_eq!(press(&mut app, &snap, &[KeyCode::Enter]), None);
        assert_eq!(app.view, View::Dashboard);
        assert_eq!(app.project.as_deref(), Some("#quest-video"));
        // Chat opened from a project starts in that project's room.
        press(&mut app, &snap, &[KeyCode::Char('c')]);
        assert_eq!(app.channel_idx, 1);
        press(&mut app, &snap, &[KeyCode::Esc, KeyCode::Esc]);
        assert_eq!(app.view, View::Home);
    }

    #[test]
    fn the_dashboard_shows_only_the_opened_project() {
        let mut snap = fixture();
        snap.projects.push(ProjectView {
            channel: "#wardrobe".into(),
            name: "Wardrobe".into(),
            ..ProjectView::default()
        });
        let mut app = App::default();
        assert!(render(&mut app, &snap).contains("#wardrobe"));
        app.project = Some("#quest-video".into());
        let screen = render(&mut app, &snap);
        assert!(screen.contains("Fix seek bar") && !screen.contains("#wardrobe"));
    }

    #[test]
    fn new_project_flow_creates_the_project_and_attaches_the_repo() {
        let snap = fixture();
        let mut app = home();
        press(&mut app, &snap, &[KeyCode::Char('n')]);
        type_text(&mut app, &snap, "Wardrobe Sim");
        press(&mut app, &snap, &[KeyCode::Enter]);
        type_text(&mut app, &snap, "C:/repos/wardrobe");
        let create = Action::Api {
            path: "/v1/projects".into(),
            body: json!({"channel": "#wardrobe-sim", "name": "Wardrobe Sim"}),
            owner: false,
            done: "Project #wardrobe-sim created".into(),
        };
        assert_eq!(
            press(&mut app, &snap, &[KeyCode::Enter]),
            Some(Action::Many(vec![
                create,
                Action::Api {
                    path: "/v1/gits".into(),
                    body: json!({
                        "name": "wardrobe-sim", "channel": "#wardrobe-sim",
                        "local_path": "C:/repos/wardrobe",
                    }),
                    owner: false,
                    done: "Project #wardrobe-sim created and its repo attached".into(),
                },
            ]))
        );

        // A test wake is a deliberate choice because it spends quota.
        press(&mut app, &snap, &[KeyCode::Char('t')]);
        assert!(render(&mut app, &snap).contains("uses that agent's quota"));
        let action = press(&mut app, &snap, &[KeyCode::Enter]);
        assert!(matches!(
            action,
            Some(Action::Api { ref path, owner: true, .. }) if path == "/v1/owner/wake"
        ));
    }

    #[test]
    fn account_status_reads_the_launcher_and_the_last_launch() {
        let with = |launcher: Option<bool>, found: bool, last: Option<&str>| Account {
            launcher,
            command_found: found,
            last_run: last.map(str::to_owned),
            ..Account::default()
        };
        let word = |account: Account| account_status(&account).0;
        assert_eq!(word(with(Some(true), true, Some("ok"))), "ready");
        assert_eq!(word(with(Some(true), true, None)), "ready");
        assert_eq!(word(with(Some(true), true, Some("out of quota"))), "out of quota");
        assert_eq!(word(with(Some(true), true, Some("failed"))), "last run failed");
        assert_eq!(word(with(Some(true), false, Some("ok"))), "command missing");
        assert_eq!(word(with(Some(false), true, Some("ok"))), "launcher off");
        assert_eq!(word(with(None, false, None)), "no launcher");
    }

    #[test]
    fn people_dots_follow_each_accounts_tasks_in_the_room() {
        let mut snap = fixture();
        let state = |snap: &Snapshot, nick: &str| member_state(snap, "#quest-video", nick);
        // codex's task is blocked on a question escalated to the owner.
        assert_eq!(state(&snap, "codex"), Some((Color::Red, "needs you")));
        snap.waiting[0].escalated = false;
        assert_eq!(state(&snap, "codex"), Some((Color::Yellow, "waiting on PM")));
        for (status, expected) in [
            ("testing", (Color::Magenta, "testing")),
            ("review", (Color::Blue, "in review")),
            ("in_progress", (Color::Green, "coding")),
            ("assigned", (Color::White, "has work")),
        ] {
            snap.projects[0].tasks[0].status = status.into();
            assert_eq!(state(&snap, "codex"), Some(expected));
        }
        // No task, but its agent is running right now.
        assert_eq!(state(&snap, "flash3.8"), Some((Color::Green, "working")));
        assert_eq!(state(&snap, "tony"), None);
    }

    #[test]
    fn the_channel_tree_counts_unread_messages_in_other_rooms() {
        let mut snap = fixture();
        let mut app = App {
            view: View::Chat,
            channel_idx: 0,
            ..App::default()
        };
        render(&mut app, &snap); // first sight: everything counts as read
        snap.messages.get_mut("#quest-video").unwrap().push(ChatLine {
            seq: 2,
            sender: "codex".into(),
            body: "done".into(),
            time: 0,
        });
        let screen = render(&mut app, &snap);
        let row = screen
            .lines()
            .find(|line| line.contains("#quest-video") && !line.contains("Message"))
            .unwrap()
            .to_string();
        assert!(row.contains(" 1"), "unread count missing in {row:?}");
        // Opening the room marks it read.
        press(&mut app, &snap, &[KeyCode::Down]);
        render(&mut app, &snap);
        assert_eq!(app.seen["#quest-video"], 2);
    }

    fn dm(seq: i64, sender: &str, body: &str) -> ChatLine {
        ChatLine {
            seq,
            sender: sender.into(),
            body: body.into(),
            time: 0,
        }
    }

    #[test]
    fn dm_peer_is_the_other_side_of_the_stored_channel() {
        assert_eq!(
            dm_peer("@dm:flash3.8:tony", "tony").as_deref(),
            Some("flash3.8")
        );
        assert_eq!(dm_peer("@dm:codex:tony", "Tony").as_deref(), Some("codex"));
        assert_eq!(dm_peer("@dm:tony:tony", "tony").as_deref(), Some("tony"));
        assert_eq!(dm_peer("#general", "tony"), None);
    }

    #[test]
    fn direct_messages_list_after_the_rooms_and_reply_privately() {
        let mut snap = fixture();
        snap.channels.push("@flash3.8".into());
        snap.messages.insert(
            "@flash3.8".into(),
            vec![dm(5, "flash3.8", "can you look at #19?")],
        );
        let mut app = App {
            view: View::Chat,
            channel_idx: 3,
            ..App::default()
        };
        let screen = render(&mut app, &snap);
        assert!(screen.contains("Direct messages"), "{screen}");
        assert!(screen.contains("can you look at #19?"), "{screen}");
        assert!(screen.contains("Message @flash3.8"), "{screen}");
        type_text(&mut app, &snap, "on it");
        assert_eq!(
            press(&mut app, &snap, &[KeyCode::Enter]),
            Some(Action::Send {
                target: "@flash3.8".into(),
                body: "on it".into()
            })
        );
        // The last room is the last DM: Down stays there.
        press(&mut app, &snap, &[KeyCode::Down]);
        assert_eq!(app.channel_idx, 3);
    }

    #[test]
    fn ctrl_d_picks_someone_and_opens_an_empty_conversation() {
        let snap = fixture();
        let mut app = App {
            view: View::Chat,
            ..App::default()
        };
        render(&mut app, &snap);
        handle_key(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL, &snap);
        let Some(Modal::Picker { items, .. }) = &app.modal else {
            panic!("no picker: {:?}", app.modal);
        };
        let nicks: Vec<&str> = items.iter().map(|(_, nick)| nick.as_str()).collect();
        assert_eq!(nicks, ["flash3.8", "flash-lite"], "you cannot DM yourself");
        press(&mut app, &snap, &[KeyCode::Down, KeyCode::Enter]);
        assert!(app.modal.is_none());
        let screen = render(&mut app, &snap);
        assert!(screen.contains("Message @flash-lite"), "{screen}");
        assert_eq!(chat_rooms(&app, &snap)[app.channel_idx], "@flash-lite");
        type_text(&mut app, &snap, "hello");
        assert_eq!(
            press(&mut app, &snap, &[KeyCode::Enter]),
            Some(Action::Send {
                target: "@flash-lite".into(),
                body: "hello".into()
            })
        );
    }

    #[test]
    fn clicking_a_person_opens_a_dm_but_not_with_yourself() {
        let snap = fixture();
        let mut app = App {
            view: View::Chat,
            ..App::default()
        };
        render(&mut app, &snap);
        assert!(app.people_rows.iter().all(|(_, nick)| nick != "tony"));
        let (row, nick) = app
            .people_rows
            .iter()
            .find(|(_, nick)| nick == "flash-lite")
            .cloned()
            .unwrap();
        let x = app.people_area.x + 2;
        handle_click(&mut app, x, row);
        let screen = render(&mut app, &snap);
        assert!(screen.contains(&format!("Message @{nick}")), "{screen}");
    }

    #[test]
    fn new_direct_messages_are_marked_unread_and_keep_your_place() {
        let mut snap = fixture();
        let mut app = App {
            view: View::Dashboard,
            ..App::default()
        };
        render(&mut app, &snap); // the hub as it was when the TUI opened
        open_dm(&mut app, "zed");
        render(&mut app, &snap);
        assert_eq!(chat_rooms(&app, &snap)[app.channel_idx], "@zed");

        // A message from someone new sorts above @zed; you stay with @zed.
        snap.channels.push("@codex".into());
        snap.messages
            .insert("@codex".into(), vec![dm(9, "codex", "need a review")]);
        let screen = render(&mut app, &snap);
        assert_eq!(chat_rooms(&app, &snap)[app.channel_idx], "@zed");
        assert!(screen.contains("[1 new direct]"), "{screen}");
        let row = screen.lines().find(|l| l.contains("@codex")).unwrap();
        let tree_row = row.split('│').nth(1).unwrap_or_default();
        assert!(
            tree_row.trim_end().ends_with(" 1"),
            "unread count missing in {row:?}"
        );

        // Your own messages never count; opening the DM marks it read.
        snap.messages
            .get_mut("@codex")
            .unwrap()
            .push(dm(10, "tony", "sure"));
        press(&mut app, &snap, &[KeyCode::Up]);
        let screen = render(&mut app, &snap);
        assert_eq!(chat_rooms(&app, &snap)[app.channel_idx], "@codex");
        assert!(!screen.contains("new direct"), "{screen}");
        assert_eq!(app.seen["@codex"], 10);
    }

    #[test]
    fn key_hints_are_bracketed_letters() {
        let screen = render(&mut App::default(), &fixture());
        for expected in ["[n] new", "[a] assign", "[o] sprint", "[Esc] home", "[q] quit"] {
            assert!(screen.contains(expected), "hints are missing {expected:?}");
        }
    }

    #[test]
    fn sprint_rooms_show_in_the_tree_on_tasks_and_filter_the_people_pane() {
        let mut snap = fixture();
        // The tree lists the room under its project, by label and title.
        let mut app = App {
            view: View::Chat,
            channel_idx: 2,
            ..App::default()
        };
        let screen = render(&mut app, &snap);
        for expected in ["[-] Quest player", "#quest-video", "s1 Local play", "Message #quest-video-s1"] {
            assert!(screen.contains(expected), "chat is missing {expected:?}");
        }
        // The project line names the current sprint and the task row its sprint.
        let dashboard = render(&mut App::default(), &snap);
        assert!(dashboard.contains("sprint s1"), "project line has no sprint");
        assert!(dashboard.contains("s1  [bug] Fix seek bar"), "task row has no sprint");

        // In a sprint room the dots come from that sprint's tasks only.
        assert_eq!(
            member_state(&snap, "#quest-video-s1", "codex"),
            Some((Color::Red, "needs you"))
        );
        snap.projects[0].tasks[0].sprint = Some("#quest-video-s2".into());
        assert_eq!(member_state(&snap, "#quest-video-s1", "codex"), None);
        assert!(member_state(&snap, "#quest-video", "codex").is_some());
    }

    #[test]
    fn o_opens_the_next_sprint_of_the_selected_project() {
        let snap = fixture();
        for mut app in [home(), App::default()] {
            press(&mut app, &snap, &[KeyCode::Char('o')]);
            assert!(render(&mut app, &snap).contains("Open the next sprint of #quest-video"));
            type_text(&mut app, &snap, "Subtitles");
            assert_eq!(
                press(&mut app, &snap, &[KeyCode::Enter]),
                sent(
                    "/v1/rooms",
                    json!({"project": "#quest-video", "title": "Subtitles"}),
                    "Sprint opened"
                )
            );
        }
        // The title is optional.
        let mut app = home();
        assert_eq!(
            press(&mut app, &snap, &[KeyCode::Char('o'), KeyCode::Enter]),
            sent(
                "/v1/rooms",
                json!({"project": "#quest-video", "title": null}),
                "Sprint opened"
            )
        );
    }

    fn with_templates(mut snap: Snapshot) -> Snapshot {
        snap.templates = vec![
            TemplateView { id: "agy".into(), name: "Antigravity".into(), installed: true },
            TemplateView { id: "lmstudio".into(), name: "LM Studio".into(), installed: false },
        ];
        snap
    }

    #[test]
    fn m_creates_an_account_from_a_client_and_model() {
        let mut snap = with_templates(fixture());
        let mut app = App::default();
        // m, then "+ create a new account", then the client.
        press(&mut app, &snap, &[KeyCode::Char('m'), KeyCode::Down, KeyCode::Down, KeyCode::Down, KeyCode::Down, KeyCode::Enter]);
        let screen = render(&mut app, &snap);
        assert!(screen.contains("New account for #quest-video: client"), "{screen}");
        assert!(screen.contains("LM Studio  (not installed)"), "{screen}");
        assert_eq!(
            press(&mut app, &snap, &[KeyCode::Enter]),
            Some(Action::LoadModels("agy".into()))
        );
        // Keys wait while the hub lists the models.
        assert!(render(&mut app, &snap).contains("Asking the hub for the models"));
        assert_eq!(press(&mut app, &snap, &[KeyCode::Down, KeyCode::Enter]), None);
        assert!(matches!(app.modal, Some(Modal::Models { .. })));

        snap.models = Some((
            "agy".into(),
            Ok(vec![
                ("gemini-4-argon-high".into(), Some("Gemini 4 Argon (High)".into())),
                ("gemini-3.8-flash-low".into(), None),
            ]),
        ));
        let screen = render(&mut app, &snap);
        assert!(screen.contains("gemini-4-argon-high  Gemini 4 Argon (High)"), "{screen}");
        assert!(screen.contains("Type a model name..."), "{screen}");
        // The account name starts as the model and can be edited.
        press(&mut app, &snap, &[KeyCode::Down, KeyCode::Enter]);
        assert!(matches!(
            app.modal,
            Some(Modal::Prompt { ref text, .. }) if text == "gemini-3.8-flash-low"
        ));
        type_text(&mut app, &snap, "-2");
        press(&mut app, &snap, &[KeyCode::Enter]);
        assert!(render(&mut app, &snap).contains("Role for @gemini-3.8-flash-low-2 in #quest-video"));
        assert_eq!(
            press(&mut app, &snap, &[KeyCode::Down, KeyCode::Enter]),
            Some(Action::CreateAccount {
                body: json!({
                    "template": "agy", "model": "gemini-3.8-flash-low",
                    "nick": "gemini-3.8-flash-low-2", "channel": "#quest-video",
                    "role": "builder",
                }),
            })
        );
        assert!(app.modal.is_none());
    }

    #[test]
    fn lmstudio_account_picker_selects_a_discovered_link_model() {
        let mut snap = with_templates(fixture());
        let mut app = App::default();
        press(&mut app, &snap, &[KeyCode::Char('m'), KeyCode::Down, KeyCode::Down, KeyCode::Down, KeyCode::Down, KeyCode::Enter, KeyCode::Down]);
        assert_eq!(
            press(&mut app, &snap, &[KeyCode::Enter]),
            Some(Action::LoadModels("lmstudio".into()))
        );
        snap.models = Some(("lmstudio".into(), Ok(vec![
            ("qwen/qwen3-coder:30b".into(), Some("Qwen 3 Coder".into())),
        ])));
        let screen = render(&mut app, &snap);
        assert!(screen.contains("qwen/qwen3-coder:30b  Qwen 3 Coder"), "{screen}");
        press(&mut app, &snap, &[KeyCode::Enter]);
        assert!(matches!(app.modal, Some(Modal::Prompt { ref title, .. })
            if title == "Account name for qwen/qwen3-coder:30b"));
        press(&mut app, &snap, &[KeyCode::Enter]);
        assert!(matches!(
            press(&mut app, &snap, &[KeyCode::Enter]),
            Some(Action::CreateAccount { ref body })
                if body["template"] == "lmstudio" && body["model"] == "qwen/qwen3-coder:30b"
        ));
    }

    #[test]
    fn a_model_the_list_does_not_show_can_be_typed() {
        let mut snap = with_templates(fixture());
        snap.models = Some(("lmstudio".into(), Err("could not start 'lms'".into())));
        let mut app = App::default();
        press(&mut app, &snap, &[KeyCode::Char('m'), KeyCode::Down, KeyCode::Down, KeyCode::Down, KeyCode::Down, KeyCode::Enter, KeyCode::Down]);
        assert_eq!(
            press(&mut app, &snap, &[KeyCode::Enter]),
            Some(Action::LoadModels("lmstudio".into()))
        );
        let screen = render(&mut app, &snap);
        assert!(screen.contains("could not start 'lms'"), "{screen}");
        press(&mut app, &snap, &[KeyCode::Enter]);
        assert!(render(&mut app, &snap).contains("Model name, exactly as the client expects it"));
        type_text(&mut app, &snap, "qwen/qwen3-coder:30b");
        press(&mut app, &snap, &[KeyCode::Enter]);
        assert!(matches!(
            app.modal,
            Some(Modal::Prompt { ref text, .. }) if text == "qwen-qwen3-coder-30b"
        ));
        press(&mut app, &snap, &[KeyCode::Enter]);
        assert!(matches!(
            press(&mut app, &snap, &[KeyCode::Enter]),
            Some(Action::CreateAccount { ref body })
                if body["model"] == "qwen/qwen3-coder:30b" && body["nick"] == "qwen-qwen3-coder-30b"
                    && body["role"] == "member"
        ));
    }

    #[test]
    fn without_templates_create_account_says_so() {
        let snap = fixture();
        let mut app = App::default();
        press(&mut app, &snap, &[KeyCode::Char('m'), KeyCode::Down, KeyCode::Down, KeyCode::Down, KeyCode::Down, KeyCode::Enter]);
        assert!(render(&mut app, &snap).contains("No client templates on the hub yet"));
        assert_eq!(press(&mut app, &snap, &[KeyCode::Enter]), None);
        assert!(app.modal.is_none());
    }

    #[test]
    fn nick_for_model_keeps_only_name_characters() {
        assert_eq!(nick_for_model("gemini-3.8-flash-medium"), "gemini-3.8-flash-medium");
        assert_eq!(nick_for_model("qwen/qwen3:30b"), "qwen-qwen3-30b");
        assert_eq!(nick_for_model("@odd model@"), "odd-model");
        assert_eq!(nick_for_model(&"x".repeat(40)).len(), 32);
    }

    #[test]
    fn save_profile_adds_one_profile_and_keeps_the_rest() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("aircd-tui-profile-{}", uuid::Uuid::new_v4()));
        save_profile(&dir, "first", "http://127.0.0.1:6680", "tok-1")?;
        let path = dir.join("profiles.json");
        let data: Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
        assert_eq!(data["default"], "first");
        // An existing file keeps its default, other profiles and other keys.
        std::fs::write(
            &path,
            r#"{"default": "tony", "extra": 1, "profiles": {"tony": {"token": "t", "nick": "tony", "server": "s"}}}"#,
        )?;
        save_profile(&dir, "new-model", "http://127.0.0.1:6680", "tok-2")?;
        let data: Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
        assert_eq!(data["default"], "tony");
        assert_eq!(data["extra"], 1);
        assert_eq!(data["profiles"]["tony"]["token"], "t");
        assert_eq!(
            data["profiles"]["new-model"],
            json!({"server": "http://127.0.0.1:6680", "token": "tok-2", "nick": "new-model"})
        );
        assert!(!dir.join("profiles.json.tmp").exists());
        // A broken file is left alone rather than replaced.
        std::fs::write(&path, "not json")?;
        assert!(save_profile(&dir, "other", "s", "tok-3").is_err());
        assert_eq!(std::fs::read_to_string(&path)?, "not json");
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[test]
    fn m_changes_a_members_role() {
        let snap = fixture();
        let mut app = App::default();
        press(&mut app, &snap, &[KeyCode::Char('m'), KeyCode::Enter]);
        assert!(render(&mut app, &snap).contains("Role for @flash3.8"));
        assert_eq!(
            press(&mut app, &snap, &[KeyCode::Down, KeyCode::Down, KeyCode::Enter]),
            sent(
                "/v1/projects/members",
                json!({"channel": "#quest-video", "nick": "flash3.8", "role": "reviewer"}),
                "@flash3.8 is now reviewer in #quest-video"
            )
        );
    }

    fn task(id: &str, title: &str, parent: Option<&str>) -> TaskView {
        TaskView {
            id: id.into(),
            kind: "task".into(),
            status: "assigned".into(),
            assignee: "codex".into(),
            title: title.into(),
            parent: parent.map(Into::into),
            ..TaskView::default()
        }
    }

    #[test]
    fn subtasks_follow_their_parent_indented_and_cycles_terminate() {
        let mut snap = fixture();
        // Listed out of order: the subtasks come before their parent.
        snap.projects[0].tasks = vec![
            task("task_child2", "Second child", Some("task_parent")),
            task("task_grand", "Grandchild", Some("task_child1")),
            task("task_child1", "First child", Some("task_parent")),
            task("task_parent", "Parent work", None),
            task("task_orphan", "Lost subtask", Some("task_gone")),
        ];
        assert_eq!(
            task_tree(&snap.projects[0]),
            vec![(3, 0), (0, 1), (2, 1), (1, 2), (4, 0)]
        );
        assert_eq!(
            rows(&snap, None)[1..],
            [
                Row::Task(0, 3),
                Row::Task(0, 0),
                Row::Task(0, 2),
                Row::Task(0, 1),
                Row::Task(0, 4)
            ]
        );

        let screen = render(&mut App::default(), &snap);
        let line = |title: &str| {
            screen
                .lines()
                .find(|l| l.contains(title))
                .unwrap_or_else(|| panic!("no row for {title:?} in\n{screen}"))
                .to_string()
        };
        let at = |title: &str| line(title).find(title).unwrap();
        assert!(!line("Parent work").contains('└'));
        assert!(line("Second child").contains("└ [task] Second child"));
        assert!(line("Grandchild").contains("  └ [task] Grandchild"));
        assert!(at("Grandchild") > at("First child"));
        assert!(at("First child") > at("Parent work"));
        // A subtask whose parent is not listed stays at the top and names it.
        assert!(line("Lost subtask").contains("sub of task_gone"));
        snap.projects[0].tasks[0].reviewer = Some("lead".into());
        let screen = render(&mut App::default(), &snap);
        assert!(screen
            .lines()
            .any(|l| l.contains("Second child") && l.contains("review @lead")));
        let order: Vec<usize> = [
            "Parent work",
            "Second child",
            "First child",
            "Grandchild",
            "Lost subtask",
        ]
        .iter()
        .map(|title| screen.find(title).unwrap())
        .collect();
        assert!(
            order.windows(2).all(|w| w[0] < w[1]),
            "rows out of order:\n{screen}"
        );

        // Selecting the second row picks the parent, not the first listed task.
        let mut app = App::default();
        press(&mut app, &snap, &[KeyCode::Down]);
        assert_eq!(selected_task(&app, &snap).unwrap().id, "task_parent");

        // A parent cycle still lists every task once.
        snap.projects[0].tasks = vec![
            task("task_a", "A", Some("task_b")),
            task("task_b", "B", Some("task_a")),
            task("task_self", "Self", Some("task_self")),
        ];
        let tree = task_tree(&snap.projects[0]);
        assert_eq!(tree, vec![(2, 0), (0, 0), (1, 1)]);
    }

    #[test]
    fn task_details_show_the_reviewer_and_the_parent_task() {
        let body = |task: Value| json!({"task": task, "events": []});
        let child = json!({
            "title": "Child", "kind": "task", "channel": "#quest-video", "status": "assigned",
            "assignee": "worker", "reviewer": "lead", "parent_task_id": "task_parent",
        });
        let parent = json!({"title": "Parent work", "assignee": "lead"});
        let lines = task_detail_lines(&body(child.clone()), Some(&parent), Some("flash3.8"));
        assert!(
            lines.contains(
                &"reviewer @lead (lead)   parent task_parent \"Parent work\"".to_string()
            ),
            "{lines:#?}"
        );
        // A parent that could not be loaded shows by id; the reviewer has no label.
        let lines = task_detail_lines(&body(child), None, None);
        assert!(
            lines.contains(&"reviewer @lead   parent task_parent".to_string()),
            "{lines:#?}"
        );

        // A top-level task is reviewed by the project manager.
        let top = json!({"title": "Top", "kind": "task", "reviewer": "flash3.8"});
        let lines = task_detail_lines(&body(top), None, Some("flash3.8"));
        assert!(
            lines.contains(&"reviewer @flash3.8 (PM)   parent -".to_string()),
            "{lines:#?}"
        );
        let none = json!({"title": "Unassigned", "kind": "task"});
        let lines = task_detail_lines(&body(none), None, Some("flash3.8"));
        assert!(
            lines.contains(&"reviewer -   parent -".to_string()),
            "{lines:#?}"
        );
    }
}
