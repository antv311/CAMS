//! `aircd tui`: a terminal dashboard and chat for the hub.
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
use ratatui::widgets::{Block, Paragraph, Wrap};
use ratatui::Frame;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

const REFRESH_EVERY: Duration = Duration::from_secs(2);
const TABS: &str = " [D]ashboard  [C]hat  [Q]uit ";
const CHANNEL_PANE_WIDTH: u16 = 24;

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
pub struct ProjectView {
    pub channel: String,
    pub name: String,
    pub pm: Option<String>,
    pub tasks: Vec<TaskView>,
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
    pub needs_you: Vec<String>,
    pub questions: Vec<String>,
    pub runs: Vec<String>,
    pub channels: Vec<String>,
    pub messages: BTreeMap<String, Vec<ChatLine>>,
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
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum View {
    Dashboard,
    Chat,
}

pub struct App {
    pub view: View,
    pub channel_idx: usize,
    pub input: String,
    pub scroll: usize,
    channel_rows: Vec<(u16, usize)>,
}

impl Default for App {
    fn default() -> Self {
        Self {
            view: View::Dashboard,
            channel_idx: 0,
            input: String::new(),
            scroll: 0,
            channel_rows: Vec::new(),
        }
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

async fn get(client: &reqwest::Client, cfg: &TuiConfig, path: &str) -> Result<Value> {
    let response = client
        .get(format!("{}{path}", cfg.server))
        .bearer_auth(&cfg.token)
        .send()
        .await
        .with_context(|| format!("cannot reach the hub at {}", cfg.server))?;
    let status = response.status();
    let body: Value = response.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        return Err(anyhow!(
            "{path}: {} {}",
            status.as_u16(),
            body["error"].as_str().unwrap_or("request failed")
        ));
    }
    Ok(body)
}

async fn refresh(
    client: &reqwest::Client,
    cfg: &TuiConfig,
    shared: &Arc<Mutex<Snapshot>>,
) -> Result<()> {
    let mut snap = shared.lock().expect("snapshot mutex poisoned").clone();
    snap.server = cfg.server.clone();
    snap.me = cfg.nick.clone();

    // Who has an agent running right now, and the recent launches.
    let mut running: HashSet<String> = HashSet::new();
    snap.runs.clear();
    if let Some(key) = cfg.owner_key.as_deref() {
        let response = client
            .post(format!("{}/v1/owner/wake", cfg.server))
            .json(&json!({"invite": key, "action": "runs", "limit": 12}))
            .send()
            .await;
        if let Ok(response) = response {
            let body: Value = response.json().await.unwrap_or(Value::Null);
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
        let tasks = get(
            client,
            cfg,
            &format!("/v1/tasks?channel={}", channel.replace('#', "%23")),
        )
        .await?;
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
            channel,
        });
    }
    snap.projects = views;

    let inbox = get(client, cfg, "/v1/inbox").await?;
    let inbox = &inbox["inbox"];
    snap.needs_you = inbox["needs_you"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|q| {
            format!(
                "{} @{}: {}",
                text(&q["channel"]),
                text(&q["asked_by"]),
                text(&q["question"])
            )
        })
        .collect();
    snap.questions = inbox["projects"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|p| p["questions"].as_array().cloned().unwrap_or_default())
        .map(|q| {
            format!(
                "{} @{}: {}",
                text(&q["channel"]),
                text(&q["asked_by"]),
                text(&q["question"])
            )
        })
        .collect();

    // Channels: #general first, then every project and any other channel.
    let listed = get(client, cfg, "/v1/channels").await?;
    let mut channels = vec!["#general".to_string()];
    for name in snap
        .projects
        .iter()
        .map(|p| p.channel.clone())
        .chain(
            listed["channels"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|c| text(&c["channel"])),
        )
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
                channel.replace('#', "%23")
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

    snap.error = None;
    *shared.lock().expect("snapshot mutex poisoned") = snap;
    Ok(())
}

async fn fetch_loop(
    cfg: TuiConfig,
    shared: Arc<Mutex<Snapshot>>,
    mut commands: UnboundedReceiver<Action>,
    stop: Arc<AtomicBool>,
) {
    let client = reqwest::Client::new();
    while !stop.load(Ordering::Relaxed) {
        tokio::select! {
            command = commands.recv() => {
                match command {
                    Some(Action::Send { target, body }) => {
                        let sent = client
                            .post(format!("{}/v1/messages", cfg.server))
                            .bearer_auth(&cfg.token)
                            .json(&json!({"target": target, "body": body}))
                            .send()
                            .await;
                        if let Err(err) = sent {
                            shared.lock().expect("snapshot mutex poisoned").error =
                                Some(format!("send failed: {err}"));
                        }
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
                    many.iter()
                        .all(|m| {
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

pub fn handle_key(
    app: &mut App,
    code: KeyCode,
    modifiers: KeyModifiers,
    snap: &Snapshot,
) -> Option<Action> {
    if modifiers.contains(KeyModifiers::CONTROL) && matches!(code, KeyCode::Char('c')) {
        return Some(Action::Quit);
    }
    match app.view {
        View::Dashboard => match code {
            KeyCode::Char('q') | KeyCode::Char('Q') => return Some(Action::Quit),
            KeyCode::Char('c') | KeyCode::Char('C') => app.view = View::Chat,
            KeyCode::Char('r') | KeyCode::Char('R') => return Some(Action::Refresh),
            KeyCode::Down | KeyCode::Char('j') => app.scroll = app.scroll.saturating_add(1),
            KeyCode::Up | KeyCode::Char('k') => app.scroll = app.scroll.saturating_sub(1),
            KeyCode::PageDown => app.scroll = app.scroll.saturating_add(10),
            KeyCode::PageUp => app.scroll = app.scroll.saturating_sub(10),
            _ => {}
        },
        View::Chat => match code {
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
                if let (false, Some(target)) = (body.is_empty(), snap.channels.get(app.channel_idx))
                {
                    return Some(Action::Send {
                        target: target.clone(),
                        body,
                    });
                }
            }
            KeyCode::Char(c) => app.input.push(c),
            _ => {}
        },
    }
    None
}

pub fn handle_click(app: &mut App, column: u16, row: u16) -> Option<Action> {
    if row == 0 {
        let offset = |label: &str| TABS.find(label).unwrap_or(0) as u16;
        let within = |label: &str| {
            let start = offset(label);
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
    if app.view == View::Chat && column < CHANNEL_PANE_WIDTH {
        if let Some((_, index)) = app.channel_rows.iter().find(|(y, _)| *y == row) {
            app.channel_idx = *index;
        }
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
        if snap.my_role.is_empty() { "?" } else { &snap.my_role }
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
        View::Dashboard => "c chat · r refresh · up/down scroll · q quit",
        View::Chat => "Enter send · Tab complete @name · up/down channel · Esc dashboard",
    };
    let footer_line = match &snap.error {
        Some(err) => Line::from(Span::styled(
            format!(" {err}"),
            Style::default().fg(Color::Red),
        )),
        None => Line::from(Span::styled(
            format!(" {hint}"),
            Style::default().fg(Color::DarkGray),
        )),
    };
    frame.render_widget(Paragraph::new(footer_line), footer);
}

fn draw_dashboard(frame: &mut Frame, app: &mut App, snap: &Snapshot, area: Rect) {
    let top_height = (snap.accounts.len() as u16 + 2).clamp(9, (area.height / 2 + 1).max(9));
    let [top, bottom] =
        Layout::vertical([Constraint::Length(top_height), Constraint::Min(4)]).areas(area);
    let [accounts_area, right] =
        Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)]).areas(top);
    let [attention_area, runs_area] =
        Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(right);

    // Accounts: running, online or offline, with the projects each manages.
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
            let managing: Vec<&str> = snap
                .projects
                .iter()
                .filter(|p| p.pm.as_deref() == Some(account.nick.to_ascii_lowercase().as_str()))
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
            Line::from(spans)
        })
        .collect();
    frame.render_widget(
        Paragraph::new(accounts).block(Block::bordered().title(" Accounts  (> working, * online) ")),
        accounts_area,
    );

    // What is waiting on a person.
    let mut attention: Vec<Line> = Vec::new();
    for item in &snap.needs_you {
        attention.push(Line::from(vec![
            Span::styled("NEEDS YOU ", bold().fg(Color::Red)),
            Span::raw(item.clone()),
        ]));
    }
    for item in &snap.questions {
        attention.push(Line::from(vec![
            Span::styled("QUESTION  ", bold().fg(Color::Yellow)),
            Span::raw(item.clone()),
        ]));
    }
    if attention.is_empty() {
        attention.push(Line::from(Span::styled(
            "Nothing is waiting on you.",
            Style::default().fg(Color::DarkGray),
        )));
    }
    frame.render_widget(
        Paragraph::new(attention)
            .wrap(Wrap { trim: false })
            .block(Block::bordered().title(" Waiting on you ")),
        attention_area,
    );

    let runs: Vec<Line> = if snap.runs.is_empty() {
        vec![Line::from(Span::styled(
            "No launches yet (or no owner key on this machine).",
            Style::default().fg(Color::DarkGray),
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
    let mut lines: Vec<Line> = Vec::new();
    for project in &snap.projects {
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
        lines.push(Line::from(vec![
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
        ]));
        for task in &project.tasks {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    format!("{:<18}", task.status),
                    Style::default().fg(status_color(&task.status)),
                ),
                Span::styled(format!("@{:<14}", task.assignee), bold()),
                Span::raw(format!("[{}] {}", task.kind, task.title)),
                Span::styled(
                    format!("  {}", task.id.chars().take(13).collect::<String>()),
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
        }
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "No projects yet. Create one with: airc project create <name>",
            Style::default().fg(Color::DarkGray),
        )));
    }
    let visible = bottom.height.saturating_sub(2) as usize;
    app.scroll = app.scroll.min(lines.len().saturating_sub(visible));
    let shown: Vec<Line> = lines.into_iter().skip(app.scroll).collect();
    frame.render_widget(
        Paragraph::new(shown).block(Block::bordered().title(" Projects and tasks ")),
        bottom,
    );
}

fn draw_chat(frame: &mut Frame, app: &mut App, snap: &Snapshot, area: Rect) {
    let [channels_area, right] =
        Layout::horizontal([Constraint::Length(CHANNEL_PANE_WIDTH), Constraint::Min(20)])
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
            app.channel_rows.push((channels_area.y + 1 + index as u16, index));
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
        let rows = (prefix.chars().count() + message.body.chars().count()).div_ceil(width).max(1);
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
            Span::styled(clock(message.time), Style::default().fg(Color::DarkGray)),
            Span::styled(format!(" {}: ", message.sender), bold().fg(name_color)),
            Span::raw(message.body.clone()),
        ]));
    }
    picked.reverse();
    if picked.is_empty() {
        picked.push(Line::from(Span::styled(
            "No messages here yet.",
            Style::default().fg(Color::DarkGray),
        )));
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
    let cursor_x = input_area.x + 1 + (app.input.chars().count() as u16).min(input_area.width.saturating_sub(3));
    frame.set_cursor_position((cursor_x, input_area.y + 1));
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
                    "aircd tui - dashboard and chat for a running hub\n\
                     Flags:\n  \
                       --server <url>     Hub address (default: the profile's, or http://127.0.0.1:6680)\n  \
                       --profile <name>   Saved airc profile to sign in as (default: your default profile)\n  \
                       --dump             Print the dashboard and chat screens once as plain text and exit\n\
                     Keys: c chat, Esc dashboard, q quit, r refresh; the mouse works on the tabs and channels."
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
        Snapshot {
            server: "http://127.0.0.1:6680".to_string(),
            me: "tony".to_string(),
            my_role: "leader".to_string(),
            accounts: vec![
                Account { nick: "tony".into(), role: "leader".into(), online: true, running: false },
                Account { nick: "flash3.8".into(), role: "builder".into(), online: false, running: true },
                Account { nick: "flash-lite".into(), role: "builder".into(), online: false, running: false },
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
            }],
            needs_you: vec!["#quest-video @codex: May I buy a licence?".into()],
            questions: vec![],
            runs: vec!["ok       @flash3.8  role in #quest-video".into()],
            channels: vec!["#general".into(), "#quest-video".into()],
            messages,
            error: None,
        }
    }

    fn render(app: &mut App, snap: &Snapshot) -> String {
        render_to_text(app, snap, 110, 32).unwrap()
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
        let mut app = App { view: View::Chat, channel_idx: 1, ..App::default() };
        app.input = "hello team".to_string();
        let screen = render(&mut app, &snap);
        for expected in ["> #quest-video", "14:05", "flash3.8: ", "Stepping in as PM", "hello team"] {
            assert!(screen.contains(expected), "chat is missing {expected:?}");
        }
    }

    #[test]
    fn keys_switch_views_send_messages_and_complete_names() {
        let snap = fixture();
        let mut app = App::default();
        let none = KeyModifiers::NONE;

        assert_eq!(handle_key(&mut app, KeyCode::Char('c'), none, &snap), None);
        assert_eq!(app.view, View::Chat);
        // In chat, letters are text: q must not quit.
        assert_eq!(handle_key(&mut app, KeyCode::Char('q'), none, &snap), None);
        assert_eq!(app.input, "q");
        app.input.clear();

        handle_key(&mut app, KeyCode::Down, none, &snap);
        for c in "ping @flash3".chars() {
            handle_key(&mut app, KeyCode::Char(c), none, &snap);
        }
        handle_key(&mut app, KeyCode::Tab, none, &snap);
        assert_eq!(app.input, "ping @flash3.8 ");
        assert_eq!(
            handle_key(&mut app, KeyCode::Enter, none, &snap),
            Some(Action::Send { target: "#quest-video".into(), body: "ping @flash3.8".into() })
        );
        assert!(app.input.is_empty());

        assert_eq!(handle_key(&mut app, KeyCode::Esc, none, &snap), None);
        assert_eq!(app.view, View::Dashboard);
        assert_eq!(handle_key(&mut app, KeyCode::Char('q'), none, &snap), Some(Action::Quit));
        assert_eq!(
            handle_key(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL, &snap),
            Some(Action::Quit)
        );
    }

    #[test]
    fn completion_extends_to_the_shared_prefix_when_several_names_match() {
        let accounts = fixture().accounts;
        assert_eq!(complete_mention("hi @fl", &accounts).as_deref(), Some("hi @flash"));
        assert_eq!(complete_mention("hi @to", &accounts).as_deref(), Some("hi @tony "));
        assert_eq!(complete_mention("hi @zz", &accounts), None);
        assert_eq!(complete_mention("no mention", &accounts), None);
    }

    #[test]
    fn clicking_the_tabs_and_a_channel_row_works() {
        let snap = fixture();
        let mut app = App::default();
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
}
