//! `aircd tui`: a terminal dashboard, chat and control panel for the hub.
//!
//! The server keeps running on its own; this mode attaches to it over the
//! local HTTP API using a saved `airc` profile. Closing the interface never
//! stops the hub or the agents it has launched.

use anyhow::{anyhow, Context, Result};
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use ratatui::Frame;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

const REFRESH_EVERY: Duration = Duration::from_secs(2);
const NOTICE_FOR: Duration = Duration::from_secs(6);
const TABS: &str = " [D]ashboard  [C]hat  [Q]uit ";
const CHANNEL_PANE_WIDTH: u16 = 24;
const STATUS_CHOICES: &[&str] = &[
    "in_progress",
    "review",
    "changes_requested",
    "merged",
    "cancelled",
];
const ROLE_CHOICES: &[&str] = &["member", "builder", "reviewer", "tester", "coordinator"];
const KIND_CHOICES: &[&str] = &["task", "bug", "feature"];
const HELP: &[&str] = &[
    "Dashboard",
    "  up/down     move between projects and tasks",
    "  Enter       show the selected task with its history",
    "  n           new task in the selected project",
    "  a           assign or reassign the selected task",
    "  s           move the selected task to another status",
    "  p           choose the selected project's manager (owner)",
    "  m           add an account to the selected project",
    "  w           answer a question that is waiting on you",
    "  l           switch an account's launcher on or off (owner)",
    "  c           chat      r  refresh      q  quit",
    "",
    "Chat",
    "  Enter send · Tab complete @name · up/down channel · Esc dashboard",
    "",
    "In a pop-up: up/down choose, Enter confirm, Esc cancel.",
];

// ---------------------------------------------------------------- data

#[derive(Clone, Debug, Default)]
pub struct Account {
    pub nick: String,
    pub role: String,
    pub online: bool,
    pub running: bool,
}

#[derive(Clone, Debug, Default)]
pub struct TaskView {
    pub id: String,
    pub kind: String,
    pub status: String,
    pub assignee: String,
    pub title: String,
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
    pub messages: BTreeMap<String, Vec<ChatLine>>,
    /// The task opened with Enter: its id and the lines to show.
    pub detail: Option<(String, Vec<String>)>,
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
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum View {
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
}

#[derive(Clone, Debug, PartialEq)]
pub enum Modal {
    /// Choose one of `items` (label, value).
    Picker { title: String, items: Vec<(String, String)>, idx: usize, flow: Flow },
    /// Type a line of text.
    Prompt { title: String, text: String, flow: Flow },
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
    channel_rows: Vec<(u16, usize)>,
    task_rows: Vec<(u16, usize)>,
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
            channel_rows: Vec::new(),
            task_rows: Vec::new(),
            scroll: 0,
        }
    }
}

/// The projects-and-tasks list, flattened in display order.
pub fn rows(snap: &Snapshot) -> Vec<Row> {
    let mut out = Vec::new();
    for (p, project) in snap.projects.iter().enumerate() {
        out.push(Row::Project(p));
        out.extend((0..project.tasks.len()).map(|t| Row::Task(p, t)));
    }
    out
}

fn selected_project<'a>(app: &App, snap: &'a Snapshot) -> Option<&'a ProjectView> {
    match rows(snap).get(app.cursor)? {
        Row::Project(p) | Row::Task(p, _) => snap.projects.get(*p),
    }
}

fn selected_task<'a>(app: &App, snap: &'a Snapshot) -> Option<&'a TaskView> {
    match rows(snap).get(app.cursor)? {
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
    snap.runs.clear();
    snap.launchers.clear();
    if let Some(key) = cfg.owner_key.as_deref() {
        let wake = |action: &str| json!({"invite": key, "action": action, "limit": 12});
        if let Ok(body) = request(client, cfg, "/v1/owner/wake", Some(&wake("runs"))).await {
            for run in body["runs"].as_array().into_iter().flatten() {
                let status = text(&run["status"]);
                let who = text(&run["principal_id"]);
                if status == "running" {
                    running.insert(who.clone());
                }
                snap.runs
                    .push(format!("{status:8} @{who}  {}", text(&run["reason"])));
            }
        }
        if let Ok(body) = request(client, cfg, "/v1/owner/wake", Some(&wake("list"))).await {
            snap.launchers = body["launchers"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|l| Launcher {
                    nick: text(&l["principal_id"]),
                    enabled: l["enabled"].as_bool().unwrap_or(false),
                    mode: text(&l["session_mode"]),
                })
                .collect();
        }
    }

    let agents = get(client, cfg, "/v1/agents").await?;
    snap.accounts = agents["agents"]
        .as_array()
        .into_iter()
        .flatten()
        // The bundled demo and test accounts are noise on a real hub.
        .filter(|a| !matches!(a["team"].as_str(), Some("demo") | Some("test")))
        .map(|a| Account {
            nick: text(&a["nick"]),
            role: text(&a["role"]),
            online: a["online"].as_bool().unwrap_or(false),
            running: running.contains(&text(&a["id"])),
        })
        .collect();
    if let Some(me) = snap
        .accounts
        .iter()
        .find(|a| a.nick.eq_ignore_ascii_case(&cfg.nick))
    {
        snap.my_role = me.role.clone();
    }

    let projects = get(client, cfg, "/v1/projects").await?;
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
    let mut channels = vec!["#general".to_string()];
    for name in snap.projects.iter().map(|p| p.channel.clone()).chain(
        listed["channels"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|c| text(&c["channel"])),
    ) {
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

    // Keep whatever an action wrote while this refresh was in flight.
    let mut guard = shared.lock().expect("snapshot mutex poisoned");
    snap.notice = guard.notice.clone();
    snap.detail = guard.detail.clone();
    snap.error = None;
    *guard = snap;
    Ok(())
}

async fn load_task(client: &reqwest::Client, cfg: &TuiConfig, id: &str) -> Result<Vec<String>> {
    let body = get(client, cfg, &format!("/v1/tasks/{id}")).await?;
    let task = &body["task"];
    let or_dash = |value: &Value| value.as_str().unwrap_or("-").to_string();
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
    Ok(lines)
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
                    Some(Action::Api { path, mut body, owner, done }) => {
                        let key = cfg.owner_key.clone();
                        if owner && key.is_none() {
                            set_notice(false, "the owner key is not on this machine".to_string());
                        } else {
                            if let (true, Some(key)) = (owner, key) {
                                body["invite"] = json!(key);
                            }
                            match request(&client, &cfg, &path, Some(&body)).await {
                                Ok(_) => set_notice(true, done),
                                Err(err) => set_notice(false, format!("{err}")),
                            }
                        }
                    }
                    Some(Action::LoadTask(id)) => {
                        let lines = load_task(&client, &cfg, &id)
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
        Flow::AddMemberRole { channel, nick } => api(
            "/v1/projects/members".to_string(),
            json!({"channel": channel, "nick": nick, "role": value}),
            "Member added",
        ),
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
    match app.view {
        View::Dashboard => handle_dashboard_key(app, code, snap),
        View::Chat => handle_chat_key(app, code, snap),
    }
}

fn handle_dashboard_key(app: &mut App, code: KeyCode, snap: &Snapshot) -> Option<Action> {
    let last = rows(snap).len().saturating_sub(1);
    match code {
        KeyCode::Char('q') | KeyCode::Char('Q') => return Some(Action::Quit),
        KeyCode::Char('c') | KeyCode::Char('C') => app.view = View::Chat,
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
                app.modal = picker(
                    format!("Add to {}", project.channel),
                    snap.accounts
                        .iter()
                        .filter(|a| {
                            !project
                                .members
                                .iter()
                                .any(|m| m.nick.eq_ignore_ascii_case(&a.nick))
                        })
                        .map(|a| (format!("@{}", a.nick), a.nick.clone()))
                        .collect(),
                    Flow::AddMember {
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
        KeyCode::Up => app.channel_idx = app.channel_idx.saturating_sub(1),
        KeyCode::Down => {
            if app.channel_idx + 1 < snap.channels.len() {
                app.channel_idx += 1;
            }
        }
        KeyCode::Backspace => {
            app.input.pop();
        }
        KeyCode::Tab => {
            if let Some(done) = complete_mention(&app.input, &snap.accounts) {
                app.input = done;
            }
        }
        KeyCode::Enter => {
            let body = app.input.trim().to_string();
            app.input.clear();
            if let (false, Some(target)) = (body.is_empty(), snap.channels.get(app.channel_idx)) {
                return Some(Action::Send {
                    target: target.clone(),
                    body,
                });
            }
        }
        KeyCode::Char(c) => app.input.push(c),
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
        if within("[D]ashboard") {
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
            }
        }
        View::Dashboard => {
            if let Some((_, index)) = app.task_rows.iter().find(|(y, _)| *y == row) {
                app.cursor = *index;
            }
        }
        _ => {}
    }
    None
}

// ---------------------------------------------------------------- drawing

fn status_color(status: &str) -> Color {
    match status {
        "blocked" => Color::Red,
        "review" => Color::Yellow,
        "changes_requested" => Color::Magenta,
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
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(TABS, bold().fg(Color::Black).bg(Color::Cyan)),
            Span::raw(who),
        ])),
        header,
    );

    match app.view {
        View::Dashboard => draw_dashboard(frame, app, snap, body),
        View::Chat => draw_chat(frame, app, snap, body),
    }

    let hint = match app.view {
        View::Dashboard => {
            "n new · a assign · s status · p PM · m member · w answer · l launchers · c chat · ? help · q quit"
        }
        View::Chat => "Enter send · Tab complete @name · up/down channel · Esc dashboard",
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
        (None, None) => Line::from(Span::styled(format!(" {hint}"), dim())),
    };
    frame.render_widget(Paragraph::new(footer_line), footer);

    if let Some(modal) = app.modal.clone() {
        draw_modal(frame, &modal, snap, body);
    }
}

fn draw_dashboard(frame: &mut Frame, app: &mut App, snap: &Snapshot, area: Rect) {
    let top_height = (snap.accounts.len() as u16 + 2).clamp(9, (area.height / 2 + 1).max(9));
    let [top, bottom] =
        Layout::vertical([Constraint::Length(top_height), Constraint::Min(4)]).areas(area);
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
            let lowered = account.nick.to_ascii_lowercase();
            let managing: Vec<&str> = snap
                .projects
                .iter()
                .filter(|p| p.pm.as_deref() == Some(lowered.as_str()))
                .map(|p| p.channel.as_str())
                .collect();
            let mut spans = vec![
                Span::styled(marker, Style::default().fg(color)),
                Span::styled(format!("{:<12} ", account.nick), bold()),
                Span::styled(
                    if account.running { "working " } else { "" },
                    Style::default().fg(Color::Cyan),
                ),
                Span::raw(account.role.clone()),
            ];
            if !managing.is_empty() {
                spans.push(Span::styled(
                    format!("  PM {}", managing.join(" ")),
                    Style::default().fg(Color::Yellow),
                ));
            }
            if snap
                .launchers
                .iter()
                .any(|l| l.nick == lowered && !l.enabled)
            {
                spans.push(Span::styled(
                    "  launcher off",
                    Style::default().fg(Color::Red),
                ));
            }
            Line::from(spans)
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

    // Projects with their tasks grouped by state: the simple sprint view.
    let all_rows = rows(snap);
    app.cursor = app.cursor.min(all_rows.len().saturating_sub(1));
    let visible = bottom.height.saturating_sub(2) as usize;
    if app.cursor < app.scroll {
        app.scroll = app.cursor;
    } else if visible > 0 && app.cursor >= app.scroll + visible {
        app.scroll = app.cursor + 1 - visible;
    }
    app.task_rows.clear();

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
            Row::Project(p) => {
                let project = &snap.projects[*p];
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
                    Span::styled(format!("   {summary}"), Style::default().fg(Color::Gray)),
                ])
            }
            Row::Task(p, t) => {
                let task = &snap.projects[*p].tasks[*t];
                Line::from(vec![
                    Span::raw("  "),
                    Span::styled(
                        format!("{:<18}", task.status),
                        Style::default().fg(status_color(&task.status)),
                    ),
                    Span::styled(format!("@{:<14}", task.assignee), bold()),
                    Span::raw(format!("[{}] {}", task.kind, task.title)),
                    Span::styled(
                        format!("  {}", task.id.chars().take(13).collect::<String>()),
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

fn draw_chat(frame: &mut Frame, app: &mut App, snap: &Snapshot, area: Rect) {
    let [channels_area, right] = Layout::horizontal([
        Constraint::Length(CHANNEL_PANE_WIDTH),
        Constraint::Min(20),
    ])
    .areas(area);
    let [messages_area, input_area] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(3)]).areas(right);

    app.channel_idx = app.channel_idx.min(snap.channels.len().saturating_sub(1));
    app.channel_rows.clear();
    let channel_lines: Vec<Line> = snap
        .channels
        .iter()
        .enumerate()
        .map(|(index, name)| {
            app.channel_rows
                .push((channels_area.y + 1 + index as u16, index));
            if index == app.channel_idx {
                Line::from(Span::styled(
                    format!("> {name}"),
                    bold().fg(Color::Black).bg(Color::Cyan),
                ))
            } else {
                Line::from(format!("  {name}"))
            }
        })
        .collect();
    frame.render_widget(
        Paragraph::new(channel_lines).block(Block::bordered().title(" Channels ")),
        channels_area,
    );

    let current = snap
        .channels
        .get(app.channel_idx)
        .cloned()
        .unwrap_or_else(|| "#general".to_string());
    let width = messages_area.width.saturating_sub(2).max(10) as usize;
    let height = messages_area.height.saturating_sub(2) as usize;
    // Take messages from the newest back until the pane is full, allowing
    // for wrapped lines.
    let mut picked: Vec<Line> = Vec::new();
    let mut used = 0usize;
    for message in snap.messages.get(&current).into_iter().flatten().rev() {
        let prefix = format!("{} {}: ", clock(message.time), message.sender);
        let rows = (prefix.chars().count() + message.body.chars().count())
            .div_ceil(width)
            .max(1);
        if used + rows > height && !picked.is_empty() {
            break;
        }
        used += rows;
        let name_color = if message.sender == "aircd" {
            Color::DarkGray
        } else if message.sender.eq_ignore_ascii_case(&snap.me) {
            Color::Green
        } else {
            Color::Cyan
        };
        picked.push(Line::from(vec![
            Span::styled(clock(message.time), dim()),
            Span::styled(format!(" {}: ", message.sender), bold().fg(name_color)),
            Span::raw(message.body.clone()),
        ]));
    }
    picked.reverse();
    if picked.is_empty() {
        picked.push(Line::from(Span::styled("No messages here yet.", dim())));
    }
    frame.render_widget(
        Paragraph::new(picked)
            .wrap(Wrap { trim: false })
            .block(Block::bordered().title(format!(" {current}  (times UTC) "))),
        messages_area,
    );

    frame.render_widget(
        Paragraph::new(app.input.clone())
            .block(Block::bordered().title(format!(" Message {current} "))),
        input_area,
    );
    let cursor_x = input_area.x
        + 1
        + (app.input.chars().count() as u16).min(input_area.width.saturating_sub(3));
    frame.set_cursor_position((cursor_x, input_area.y + 1));
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
    let mut app = App::default();

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
                Event::Mouse(mouse) if mouse.kind == MouseEventKind::Down(MouseButton::Left) => {
                    handle_click(&mut app, mouse.column, mouse.row)
                }
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
        };
        Snapshot {
            server: "http://127.0.0.1:6680".to_string(),
            me: "tony".to_string(),
            my_role: "leader".to_string(),
            accounts: vec![
                account("tony", "leader", true, false),
                account("flash3.8", "builder", false, true),
                account("flash-lite", "builder", false, false),
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
            channels: vec!["#general".into(), "#quest-video".into()],
            messages,
            detail: None,
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
            "PM #quest-video",
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
            "> #quest-video",
            "14:05",
            "flash3.8: ",
            "Stepping in as PM",
            "hello team",
        ] {
            assert!(screen.contains(expected), "chat is missing {expected:?}");
        }
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
            &[KeyCode::Char('s'), KeyCode::Down, KeyCode::Down, KeyCode::Enter],
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
        // Add member: only accounts that are not members yet are offered.
        press(&mut app, &snap, &[KeyCode::Char('m')]);
        let screen = render(&mut app, &snap);
        assert!(screen.contains("@flash-lite") && screen.contains("Add to #quest-video"));
        assert_eq!(
            press(
                &mut app,
                &snap,
                &[KeyCode::Down, KeyCode::Enter, KeyCode::Down, KeyCode::Enter]
            ),
            sent(
                "/v1/projects/members",
                json!({"channel": "#quest-video", "nick": "flash-lite", "role": "builder"}),
                "Member added"
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
}
