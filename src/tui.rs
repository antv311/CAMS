//! `aircd tui`: a terminal dashboard, chat and control panel for the hub.
//!
//! The server keeps running on its own; this mode attaches to it over the
//! local HTTP API using a saved `airc` profile. Closing the interface never
//! stops the hub or the agents it has launched.

use crate::config::config_dir;
use crate::history::ParsedSearchQuery;
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
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

const REFRESH_EVERY: Duration = Duration::from_secs(2);
const NOTICE_FOR: Duration = Duration::from_secs(6);
const TABS: &str = " [H]ome  [D]ashboard  [G]roups  [C]hat  [Q]uit ";
const CHANNEL_PANE_WIDTH: u16 = 26;
const PEOPLE_PANE_WIDTH: u16 = 26;
const GROUPS_PANE_WIDTH: u16 = 34;
const STATUS_CHOICES: &[&str] = &[
    "in_progress",
    "testing",
    "review",
    "changes_requested",
    "merged",
    "cancelled",
    "super_review",
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
    "  g           groups    c  chat      r  refresh      q  quit",
    "  / / Ctrl+F  search chat history across sprint channels and DMs",
    "  S           start the hub on this machine when it is not running",
    "              (Ctrl+S in chat)",
    "",
    "Groups  (g from Home/Dashboard)",
    "  n / e       create a new group / edit the selected group",
    "  a           attach the selected group to a project",
    "  Tab / <- -> switch between Groups and Current Subtasks",
    "  up/down     move within the focused pane",
    "  Enter       open the selected subtask details",
    "  d / Esc     dashboard   c  chat   r  refresh   q  quit",
    "",
    "Chat",
    "  Enter send · Tab complete @name · up/down channel · Esc dashboard",
    "  PageUp/PageDown scroll history · Home oldest · End latest",
    "  Mouse wheel scrolls history over the message pane",
    "  Ctrl+D or click a name in People: direct message someone",
    "  Ctrl+F: search conversation history (sprint channels & DMs)",
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
    pub priority: String,
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

/// A lead or worker seat in a group definition.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GroupSeatView {
    pub template: String,
    pub model: String,
    pub count: u32,
    pub nick: Option<String>,
}

/// Peer review configuration for a group.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GroupReviewPolicyView {
    pub peer_reviewers: Vec<String>,
    pub quorum: Option<u32>,
}

/// A group definition and its attached projects.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GroupView {
    pub name: String,
    pub description: Option<String>,
    pub lead: GroupSeatView,
    pub workers: Vec<GroupSeatView>,
    pub projects: Vec<String>,
    pub lead_prompt: Option<String>,
    pub worker_prompt: Option<String>,
    pub review_policy: Option<GroupReviewPolicyView>,
    pub inactivity_timeout_secs: Option<i64>,
}

/// A model a client can run: its id and, when the client gave one, a label.
pub type ModelItem = (String, Option<String>);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SearchScope {
    #[default]
    All,
    Sprints,
    DirectMessages,
    CurrentRoom,
}

impl SearchScope {
    fn api_param(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Sprints => "sprints",
            Self::DirectMessages => "dms",
            Self::CurrentRoom => "channel",
        }
    }

    fn next(self, has_room: bool) -> Self {
        match self {
            Self::All => Self::Sprints,
            Self::Sprints => Self::DirectMessages,
            Self::DirectMessages => {
                if has_room {
                    Self::CurrentRoom
                } else {
                    Self::All
                }
            }
            Self::CurrentRoom => Self::All,
        }
    }

    fn prev(self, has_room: bool) -> Self {
        match self {
            Self::All => {
                if has_room {
                    Self::CurrentRoom
                } else {
                    Self::DirectMessages
                }
            }
            Self::Sprints => Self::All,
            Self::DirectMessages => Self::Sprints,
            Self::CurrentRoom => Self::DirectMessages,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchHit {
    pub room: String,
    pub seq: i64,
    pub sender: String,
    pub body: String,
    pub time: i64,
    pub is_sprint: bool,
    pub is_dm: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchResponse {
    pub query: String,
    pub scope: SearchScope,
    pub channel: Option<String>,
    pub hits: Vec<SearchHit>,
    pub error: Option<String>,
}

/// Everything the screens show, refreshed from the server in the background.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub server: String,
    pub me: String,
    pub my_role: String,
    pub accounts: Vec<Account>,
    pub projects: Vec<ProjectView>,
    pub groups: Vec<GroupView>,
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
    /// Results from the hub's `/v1/messages/search` endpoint for the active search.
    pub search_results: Option<SearchResponse>,
    /// The result of the last action: (succeeded, text, when).
    pub notice: Option<(bool, String, Instant)>,
    /// The hub could not be reached or refused the refresh.
    pub error: Option<String>,
    /// Nothing is listening at the hub's address, so it can be started.
    pub hub_down: bool,
}

#[derive(Clone, Debug, Default)]
struct TuiConfig {
    server: String,
    token: String,
    nick: String,
    owner_key: Option<String>,
    tls_ca: Option<String>,
    tls_cert: Option<String>,
    tls_key: Option<String>,
    tls_insecure: bool,
}

#[derive(Clone, Debug, Default)]
struct TuiTlsOverrides {
    ca: Option<String>,
    cert: Option<String>,
    key: Option<String>,
    insecure: bool,
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
    /// Query `/v1/messages/search` for older or full-database chat matches.
    SearchHistory {
        query: String,
        scope: SearchScope,
        channel: Option<String>,
    },
    /// Load a historical message window around `around_seq` for jumping to old hits.
    LoadMessageWindow {
        room: String,
        around_seq: i64,
    },
    /// Start the hub on this machine because nothing answers at its address.
    StartHub,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum View {
    Home,
    Dashboard,
    Groups,
    Chat,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum GroupsFocus {
    #[default]
    Groups,
    Subtasks,
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupEditorSeat {
    pub template: String,
    pub model: String,
    pub count: u32,
    pub nick: Option<String>,
}

impl From<&GroupSeatView> for GroupEditorSeat {
    fn from(seat: &GroupSeatView) -> Self {
        Self {
            template: seat.template.clone(),
            model: seat.model.clone(),
            count: seat.count.max(1),
            nick: seat.nick.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupEditorField {
    Name,
    Description,
    Lead,
    Worker(usize),
    AddWorker,
    TimeoutSecs,
    ReviewPeers,
    ReviewQuorum,
    LeadPrompt,
    WorkerPrompt,
    Save,
    Cancel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeatStep {
    Menu { row: usize },
    PickTemplate { idx: usize },
    TypeTemplate,
    PickModel { idx: usize },
    TypeModel,
    TypeCount,
    PickNick { idx: usize },
    TypeNick,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GroupSeatEditor {
    /// `None` edits the lead seat; `Some(i)` edits worker `i` (`i == workers.len()` appends).
    pub target: Option<usize>,
    pub seat: GroupEditorSeat,
    pub text: String,
    pub step: SeatStep,
    pub wizard: bool,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum GroupEditorSub {
    Text { field: GroupEditorField, text: String },
    Seat(GroupSeatEditor),
}

#[derive(Clone, Debug, PartialEq)]
pub struct GroupEditorState {
    pub is_edit: bool,
    pub name: String,
    pub description: String,
    pub lead: GroupEditorSeat,
    pub workers: Vec<GroupEditorSeat>,
    pub timeout_secs: String,
    pub review_peers: String,
    pub review_quorum: String,
    pub lead_prompt: String,
    pub worker_prompt: String,
    pub cursor: usize,
    pub sub: Option<GroupEditorSub>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchState {
    pub query: String,
    pub scope: SearchScope,
    pub current_room: Option<String>,
    pub idx: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Modal {
    /// Choose one of `items` (label, value).
    Picker { title: String, items: Vec<(String, String)>, idx: usize, flow: Flow },
    /// Type a line of text.
    Prompt { title: String, text: String, flow: Flow },
    /// Choose a model of `template`; the list arrives from the hub.
    Models { channel: String, template: String, idx: usize },
    /// Interactive form to create or edit a group definition.
    GroupEditor(Box<GroupEditorState>),
    /// Interactive dialog to attach a group to one or more projects.
    GroupAttach {
        group_name: String,
        attached: Vec<String>,
        selected: Vec<String>,
        idx: usize,
    },
    /// Full-text search across sprint channels and direct messages.
    Search(Box<SearchState>),
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
    /// Selected group index and scroll offset in `View::Groups`.
    pub group_idx: usize,
    pub group_scroll: usize,
    /// Selected subtask index and scroll offset in `View::Groups`.
    pub group_subtask_cursor: usize,
    pub group_subtask_scroll: usize,
    pub groups_focus: GroupsFocus,
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
    /// Target `(room, seq)` to scroll into view on the next chat render.
    pub jump_to_seq: Option<(String, i64)>,
    /// Currently highlighted `(room, seq)` jumped to from search.
    pub chat_highlight: Option<(String, i64)>,
    channel_rows: Vec<(u16, usize)>,
    task_rows: Vec<(u16, usize)>,
    home_rows: Vec<(u16, usize)>,
    group_rows: Vec<(u16, usize)>,
    group_subtask_rows: Vec<(u16, usize)>,
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
            group_idx: 0,
            group_scroll: 0,
            group_subtask_cursor: 0,
            group_subtask_scroll: 0,
            groups_focus: GroupsFocus::Groups,
            seen: HashMap::new(),
            seen_floor: None,
            dm_open: Vec::new(),
            chat_scroll: 0,
            chat_max_scroll: 0,
            chat_content_height: 0,
            chat_area: Rect::default(),
            chat_channel: None,
            jump_to_seq: None,
            chat_highlight: None,
            channel_rows: Vec::new(),
            task_rows: Vec::new(),
            home_rows: Vec::new(),
            group_rows: Vec::new(),
            group_subtask_rows: Vec::new(),
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
    app.jump_to_seq = None;
    app.chat_highlight = None;
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

fn clean_nick(nick: &str) -> &str {
    nick.trim().trim_start_matches('@')
}

fn nicks_match(a: &str, b: &str) -> bool {
    clean_nick(a).eq_ignore_ascii_case(clean_nick(b))
}

fn is_terminal_status(status: &str) -> bool {
    matches!(status, "done" | "merged" | "cancelled" | "skipped")
}

/// Rank a task's activity state for member/worker status indicators.
fn task_activity_rank(snap: &Snapshot, task: &TaskView) -> (u8, Color, &'static str) {
    // An open question counts whatever status the task shows.
    let question = snap.waiting.iter().find(|w| w.task_id == task.id);
    match (question, task.status.as_str()) {
        (Some(w), _) if w.escalated => (0, Color::Red, "needs you"),
        (Some(_), _) | (None, "blocked") => (1, Color::Yellow, "waiting on PM"),
        (None, "testing") => (2, Color::Magenta, "testing"),
        (None, "review") => (3, Color::Blue, "in review"),
        (None, "super_review") => (3, Color::Blue, "PM review"),
        (None, "changes_requested") => (3, Color::Yellow, "changes req"),
        (None, "in_progress" | "claimed") => (4, Color::Green, "coding"),
        _ => (5, Color::White, "has work"),
    }
}

/// Live activity summary for a named group member across the group's projects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupMemberState {
    pub color: Color,
    pub label: String,
    pub active: bool,
}

/// Compute a group member's live state from tasks in the group's attached
/// projects and the member's account/launcher state.
pub fn group_member_state(snap: &Snapshot, group: &GroupView, nick: &str) -> GroupMemberState {
    let mut best: Option<(u8, Color, &'static str)> = None;
    for project in snap.projects.iter().filter(|p| {
        group
            .projects
            .iter()
            .any(|ch| ch.eq_ignore_ascii_case(&p.channel))
    }) {
        for task in project
            .tasks
            .iter()
            .filter(|t| !is_terminal_status(&t.status) && nicks_match(&t.assignee, nick))
        {
            let state = task_activity_rank(snap, task);
            if best.is_none_or(|current| state.0 < current.0) {
                best = Some(state);
            }
        }
    }
    let account = snap.accounts.iter().find(|a| nicks_match(&a.nick, nick));
    if account.is_some_and(|a| a.running) && best.is_none_or(|(rank, _, _)| rank > 4) {
        return GroupMemberState {
            color: Color::Green,
            label: "working".to_string(),
            active: true,
        };
    }
    if let Some((_, color, label)) = best {
        return GroupMemberState {
            color,
            label: label.to_string(),
            active: true,
        };
    }
    match account {
        Some(acc) if acc.online => GroupMemberState {
            color: Color::Green,
            label: "online".to_string(),
            active: false,
        },
        Some(acc) => {
            let (status, color) = account_status(acc);
            GroupMemberState {
                color,
                label: status,
                active: false,
            }
        }
        None => GroupMemberState {
            color: Color::DarkGray,
            label: "unregistered".to_string(),
            active: false,
        },
    }
}

/// A non-terminal subtask associated with a group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupSubtaskItem {
    pub task_id: String,
    pub channel: String,
    pub status: String,
    pub priority: String,
    pub kind: String,
    pub assignee: String,
    pub title: String,
    pub parent_id: String,
    pub parent_title: Option<String>,
    pub reviewer: Option<String>,
}

fn group_named_members(group: &GroupView) -> Vec<&str> {
    std::iter::once(group.lead.nick.as_deref())
        .chain(group.workers.iter().map(|w| w.nick.as_deref()))
        .flatten()
        .map(clean_nick)
        .filter(|n| !n.is_empty())
        .collect()
}

/// Current (non-terminal) subtasks belonging to `group` across its attached projects.
pub fn group_subtasks(snap: &Snapshot, group: &GroupView) -> Vec<GroupSubtaskItem> {
    let members = group_named_members(group);
    let is_member = |nick: &str| members.iter().any(|m| nicks_match(m, nick));
    let lead_nick = group.lead.nick.as_deref().map(clean_nick);
    let mut out = Vec::new();
    for project in snap.projects.iter().filter(|p| {
        group
            .projects
            .iter()
            .any(|ch| ch.eq_ignore_ascii_case(&p.channel))
    }) {
        for task in &project.tasks {
            let Some(parent_id) = task.parent.as_deref() else {
                continue;
            };
            if is_terminal_status(&task.status) {
                continue;
            }
            let parent = project.tasks.iter().find(|t| t.id == parent_id);
            let belongs = (!task.assignee.is_empty()
                && task.assignee != "-"
                && is_member(&task.assignee))
                || (lead_nick.is_some()
                    && task
                        .reviewer
                        .as_deref()
                        .is_some_and(|r| nicks_match(r, lead_nick.unwrap())))
                || parent.is_some_and(|p| is_member(&p.assignee));
            if !belongs {
                continue;
            }
            out.push(GroupSubtaskItem {
                task_id: task.id.clone(),
                channel: project.channel.clone(),
                status: task.status.clone(),
                priority: task.priority.clone(),
                kind: task.kind.clone(),
                assignee: task.assignee.clone(),
                title: task.title.clone(),
                parent_id: parent_id.to_string(),
                parent_title: parent.map(|p| p.title.clone()),
                reviewer: task.reviewer.clone(),
            });
        }
    }
    out
}

/// Non-terminal tasks assigned to a specific member across `group`'s attached projects.
fn group_member_tasks<'a>(snap: &'a Snapshot, group: &GroupView, nick: &str) -> Vec<&'a TaskView> {
    let mut tasks = Vec::new();
    for project in snap.projects.iter().filter(|p| {
        group
            .projects
            .iter()
            .any(|ch| ch.eq_ignore_ascii_case(&p.channel))
    }) {
        for task in &project.tasks {
            if !is_terminal_status(&task.status) && nicks_match(&task.assignee, nick) {
                tasks.push(task);
            }
        }
    }
    tasks
}

/// Count `(active_named_workers, total_worker_seats)` for a group.
pub fn group_active_worker_count(snap: &Snapshot, group: &GroupView) -> (usize, usize) {
    let total: usize = group.workers.iter().map(|w| w.count as usize).sum();
    let mut seen: Vec<String> = Vec::new();
    let mut active = 0usize;
    for worker in &group.workers {
        let Some(nick) = worker.nick.as_deref().map(clean_nick) else {
            continue;
        };
        if nick.is_empty() || seen.iter().any(|s| s.eq_ignore_ascii_case(nick)) {
            continue;
        }
        seen.push(nick.to_string());
        if group_member_state(snap, group, nick).active {
            active += 1;
        }
    }
    (active, total)
}

fn select_project_group(app: &mut App, snap: &Snapshot) {
    let Some(channel) = app.project.as_deref() else {
        return;
    };
    let current_matches = snap.groups.get(app.group_idx).is_some_and(|g| {
        g.projects
            .iter()
            .any(|p| p.eq_ignore_ascii_case(channel))
    });
    if !current_matches {
        if let Some(pos) = snap.groups.iter().position(|g| {
            g.projects
                .iter()
                .any(|p| p.eq_ignore_ascii_case(channel))
        }) {
            app.group_idx = pos;
            app.group_subtask_cursor = 0;
            app.group_subtask_scroll = 0;
        }
    }
}

// ---------------------------------------------------------------- config

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn load_config(
    server: Option<String>,
    profile: Option<String>,
    tls: TuiTlsOverrides,
) -> Result<TuiConfig> {
    let dir = config_dir();
    load_config_from_dir(&dir, server, profile, tls, env_nonempty)
}

fn load_config_from_dir(
    dir: &Path,
    server: Option<String>,
    profile: Option<String>,
    tls: TuiTlsOverrides,
    env: impl Fn(&str) -> Option<String>,
) -> Result<TuiConfig> {
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
    let tls_ca = tls
        .ca
        .or_else(|| env("AIRCD_TLS_CA"))
        .or_else(|| entry["tls_ca"].as_str().map(str::to_owned))
        .filter(|s| !s.trim().is_empty());
    let tls_cert = tls
        .cert
        .or_else(|| env("AIRCD_TLS_CLIENT_CERT"))
        .or_else(|| entry["tls_cert"].as_str().map(str::to_owned))
        .filter(|s| !s.trim().is_empty());
    let tls_key = tls
        .key
        .or_else(|| env("AIRCD_TLS_CLIENT_KEY"))
        .or_else(|| entry["tls_key"].as_str().map(str::to_owned))
        .filter(|s| !s.trim().is_empty());
    if tls_key.is_some() && tls_cert.is_none() {
        return Err(anyhow!("--tls-cert is required when --tls-key is specified"));
    }
    let tls_insecure = tls.insecure
        || env("AIRCD_TLS_INSECURE")
            .is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        || entry["tls_insecure"].as_bool().unwrap_or(false);
    Ok(TuiConfig {
        server: server
            .or_else(|| entry["server"].as_str().map(str::to_owned))
            .unwrap_or_else(|| "http://127.0.0.1:6680".to_string())
            .trim_end_matches('/')
            .to_string(),
        token,
        nick: entry["nick"].as_str().unwrap_or(&name).to_string(),
        owner_key,
        tls_ca,
        tls_cert,
        tls_key,
        tls_insecure,
    })
}

fn build_http_client(cfg: &TuiConfig) -> Result<reqwest::Client> {
    crate::tls::ensure_crypto_provider();
    if cfg.tls_key.is_some() && cfg.tls_cert.is_none() {
        return Err(anyhow!("tls_cert is required when tls_key is set"));
    }
    let mut builder = reqwest::Client::builder();
    if let Some(ca_path) = &cfg.tls_ca {
        let ca_pem = std::fs::read(ca_path)
            .with_context(|| format!("read TLS CA certificate: {ca_path}"))?;
        let cert = reqwest::Certificate::from_pem(&ca_pem)
            .with_context(|| format!("parse TLS CA certificate: {ca_path}"))?;
        builder = builder.add_root_certificate(cert);
    }
    if let Some(cert_path) = &cfg.tls_cert {
        let mut combined = std::fs::read(cert_path)
            .with_context(|| format!("read client TLS certificate: {cert_path}"))?;
        if let Some(key_path) = &cfg.tls_key {
            let key_pem = std::fs::read(key_path)
                .with_context(|| format!("read client TLS private key: {key_path}"))?;
            if !combined.ends_with(b"\n") {
                combined.push(b'\n');
            }
            combined.extend_from_slice(&key_pem);
        }
        let identity = reqwest::Identity::from_pem(&combined)
            .with_context(|| format!("load client TLS identity from {cert_path}"))?;
        builder = builder.identity(identity);
    }
    if cfg.tls_insecure {
        builder = builder.danger_accept_invalid_certs(true);
    }
    builder.build().context("build HTTP/HTTPS client")
}

// ---------------------------------------------------------------- fetching

fn text(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_string()
}

fn encode_channel(channel: &str) -> String {
    channel.replace('#', "%23")
}

fn encode_path_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for b in segment.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
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

fn group_seat_view(value: &Value) -> GroupSeatView {
    GroupSeatView {
        template: text(&value["template"]),
        model: text(&value["model"]),
        count: value["count"].as_u64().unwrap_or(1) as u32,
        nick: value["nick"]
            .as_str()
            .map(clean_nick)
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
    }
}

fn group_view(value: &Value) -> GroupView {
    let review_policy = value.get("review_policy").and_then(|rp| {
        rp.as_object().map(|_| GroupReviewPolicyView {
            peer_reviewers: rp["peer_reviewers"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str())
                .map(clean_nick)
                .map(str::to_owned)
                .collect(),
            quorum: rp["quorum"].as_u64().map(|q| q as u32),
        })
    });
    GroupView {
        name: text(&value["name"]),
        description: value["description"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
        lead: group_seat_view(&value["lead"]),
        workers: value["workers"]
            .as_array()
            .into_iter()
            .flatten()
            .map(group_seat_view)
            .collect(),
        projects: value["projects"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| p.as_str().map(str::to_owned))
            .collect(),
        lead_prompt: value["lead_prompt"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
        worker_prompt: value["worker_prompt"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
        review_policy,
        inactivity_timeout_secs: value["inactivity_timeout_secs"].as_i64(),
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

    // Older hubs may not expose /v1/groups; fall back to an empty list.
    snap.groups = match get(client, cfg, "/v1/groups").await {
        Ok(body) => body["groups"]
            .as_array()
            .into_iter()
            .flatten()
            .map(group_view)
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
                    priority: t["priority"].as_str().unwrap_or("normal").to_string(),
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
    snap.search_results = guard.search_results.clone();
    snap.error = None;
    snap.hub_down = false;
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
    let priority_suffix = task["priority"]
        .as_str()
        .map(|p| format!("   priority {p}"))
        .unwrap_or_default();
    let mut lines = vec![
        format!("{}  [{}]", text(&task["title"]), text(&task["kind"])),
        format!(
            "{}   status {}{}   assignee {}",
            text(&task["channel"]),
            text(&task["status"]),
            priority_suffix,
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
    if let Some(deps) = body["dependencies"].as_array().filter(|d| !d.is_empty()) {
        lines.push("Blocked by:".to_string());
        for dep in deps {
            let state = if dep["resolved"].as_bool() == Some(true) {
                "resolved"
            } else {
                "blocking"
            };
            lines.push(format!(
                "  {} [{}/{}] repo={} {}: {}",
                text(&dep["task_id"]),
                text(&dep["status"]),
                state,
                or_dash(&dep["repo"]),
                text(&dep["channel"]),
                text(&dep["title"])
            ));
        }
        lines.push(String::new());
    } else if let Some(ids) = task["blocked_by"].as_array().filter(|d| !d.is_empty()) {
        lines.push(format!(
            "blocked_by: {}",
            ids.iter().map(text).collect::<Vec<_>>().join(", ")
        ));
        lines.push(String::new());
    }
    if let Some(round) = task["review_round"].as_object() {
        lines.push(format!(
            "Peer review {}   commit {}   quorum {}",
            text(&round["revision"]),
            text(&round["commit"]),
            round["quorum"]
        ));
        if let Some(peers) = round["peer_reviewers"].as_array() {
            lines.push(format!(
                "Peers: {}",
                peers.iter().map(text).collect::<Vec<_>>().join(", ")
            ));
        }
        if let Some(votes) = round["votes"].as_array() {
            for vote in votes {
                lines.push(format!(
                    "  @{}: {} - {}",
                    text(&vote["reviewer"]),
                    text(&vote["decision"]),
                    text(&vote["note"])
                ));
            }
        }
        lines.push(String::new());
    }
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

/// The `--http-bind` address for a hub this machine can start itself:
/// only a hub on the loopback address, at the port in its URL.
pub fn hub_bind_for(server: &str) -> Option<String> {
    let url = reqwest::Url::parse(server).ok()?;
    if url.scheme() != "http" {
        return None;
    }
    let port = url.port_or_known_default()?;
    let host = url.host_str()?;
    if host.eq_ignore_ascii_case("localhost") {
        return Some(format!("127.0.0.1:{port}"));
    }
    let ip: std::net::IpAddr = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse()
        .ok()?;
    ip.is_loopback()
        .then(|| std::net::SocketAddr::new(ip, port).to_string())
}

/// True when the request never reached a listening hub (connection refused
/// or no route), as opposed to the hub answering with an error.
fn hub_unreachable(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<reqwest::Error>()
            .is_some_and(|e| e.is_connect())
    })
}

/// Start `aircd` in the background on the hub's address, with the same
/// database folder `scripts/Start-Aircd.ps1` uses. The hub keeps running
/// after the interface closes. Returns what to tell the user.
async fn start_hub(server: &str) -> (bool, String) {
    let Some(http_bind) = hub_bind_for(server) else {
        return (
            false,
            format!("{server} is not on this machine; start that hub there"),
        );
    };
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(err) => return (false, format!("cannot find the aircd program: {err}")),
    };
    let dir = config_dir();
    if let Err(err) = std::fs::create_dir_all(&dir) {
        return (false, format!("cannot create {}: {err}", dir.display()));
    }
    let log = dir.join("aircd.stdout.log");
    let err_log = dir.join("aircd.stderr.log");
    let (Ok(out), Ok(errs)) = (std::fs::File::create(&log), std::fs::File::create(&err_log)) else {
        return (
            false,
            format!("cannot write the hub log in {}", dir.display()),
        );
    };
    let mut command = std::process::Command::new(exe);
    command
        .arg("--http-bind")
        .arg(&http_bind)
        .arg("--db")
        .arg(dir.join("aircd.sqlite3"))
        .stdin(std::process::Stdio::null())
        .stdout(out)
        .stderr(errs);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // No console window, and Ctrl+C here does not reach the hub.
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => return (false, format!("could not start the hub: {err}")),
    };
    // A hub that cannot bind its ports or open its database exits at once.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    match child.try_wait() {
        Ok(Some(status)) => (
            false,
            format!(
                "the hub stopped at once ({status}); see {}",
                err_log.display()
            ),
        ),
        _ => (
            true,
            format!(
                "started the hub (PID {}) on {http_bind}; log in {}",
                child.id(),
                dir.display()
            ),
        ),
    }
}

async fn search_history_api(
    client: &reqwest::Client,
    cfg: &TuiConfig,
    snap: &Snapshot,
    query: &str,
    scope: SearchScope,
    channel: Option<&str>,
) -> SearchResponse {
    let mut path = format!(
        "/v1/messages/search?q={}&scope={}&limit=100",
        encode_path_segment(query),
        scope.api_param()
    );
    if scope == SearchScope::CurrentRoom {
        if let Some(ch) = channel {
            path.push_str("&channel=");
            path.push_str(&encode_path_segment(ch));
        }
    }
    let url = format!("{}{path}", cfg.server);
    let response = match client.get(url).bearer_auth(&cfg.token).send().await {
        Ok(resp) => resp,
        Err(err) => {
            return SearchResponse {
                query: query.to_string(),
                scope,
                channel: channel.map(str::to_owned),
                hits: Vec::new(),
                error: Some(format!("{err}")),
            };
        }
    };
    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        // Older hubs do not expose /v1/messages/search; client-side search stays active.
        return SearchResponse {
            query: query.to_string(),
            scope,
            channel: channel.map(str::to_owned),
            hits: Vec::new(),
            error: None,
        };
    }
    let parsed: Value = response.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        let msg = parsed["error"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| format!("HTTP {}", status.as_u16()));
        return SearchResponse {
            query: query.to_string(),
            scope,
            channel: channel.map(str::to_owned),
            hits: Vec::new(),
            error: Some(msg),
        };
    }
    let my_id = snap
        .accounts
        .iter()
        .find(|a| a.nick.eq_ignore_ascii_case(&cfg.nick))
        .map(|a| a.id.clone())
        .unwrap_or_else(|| cfg.nick.clone());
    let mut hits = Vec::new();
    for msg in parsed["messages"].as_array().into_iter().flatten() {
        let raw_chan = text(&msg["channel"]);
        let (room, is_dm, is_sprint) = if let Some(peer) = dm_peer(&raw_chan, &my_id) {
            let nick = snap
                .accounts
                .iter()
                .find(|a| a.id.eq_ignore_ascii_case(&peer))
                .map(|a| a.nick.clone())
                .unwrap_or(peer);
            (format!("@{nick}"), true, false)
        } else {
            let sprint = project_for(snap, &raw_chan).is_some_and(|p| p.channel != raw_chan);
            (raw_chan, false, sprint)
        };
        hits.push(SearchHit {
            room,
            seq: msg["seq"].as_i64().unwrap_or(0),
            sender: text(&msg["sender_id"]),
            body: text(&msg["body"]),
            time: msg["created_at"].as_i64().unwrap_or(0),
            is_sprint,
            is_dm,
        });
    }
    SearchResponse {
        query: query.to_string(),
        scope,
        channel: channel.map(str::to_owned),
        hits,
        error: None,
    }
}

async fn load_message_window(
    client: &reqwest::Client,
    cfg: &TuiConfig,
    shared: &Arc<Mutex<Snapshot>>,
    room: &str,
    around_seq: i64,
) {
    if room.starts_with('@') {
        return;
    }
    let from_seq = around_seq.saturating_sub(200).max(1);
    let path = format!(
        "/v1/messages?channel={}&from_seq={from_seq}&limit=500",
        encode_channel(room)
    );
    if let Ok(fetched) = get(client, cfg, &path).await {
        let mut guard = shared.lock().expect("snapshot mutex poisoned");
        let lines = guard.messages.entry(room.to_string()).or_default();
        let mut seen: HashSet<i64> = lines.iter().map(|l| l.seq).collect();
        for message in fetched["messages"].as_array().into_iter().flatten() {
            let seq = message["seq"].as_i64().unwrap_or(0);
            if seen.insert(seq) {
                lines.push(ChatLine {
                    seq,
                    sender: text(&message["sender_id"]),
                    body: text(&message["body"]),
                    time: message["created_at"].as_i64().unwrap_or(0),
                });
            }
        }
        lines.sort_by_key(|l| l.seq);
    }
}

async fn fetch_loop(
    client: reqwest::Client,
    cfg: TuiConfig,
    shared: Arc<Mutex<Snapshot>>,
    mut commands: UnboundedReceiver<Action>,
    stop: Arc<AtomicBool>,
) {
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
                    Some(Action::SearchHistory { query, scope, channel }) => {
                        let snap = shared.lock().expect("snapshot mutex poisoned").clone();
                        let result = search_history_api(
                            &client,
                            &cfg,
                            &snap,
                            &query,
                            scope,
                            channel.as_deref(),
                        )
                        .await;
                        shared.lock().expect("snapshot mutex poisoned").search_results =
                            Some(result);
                        continue;
                    }
                    Some(Action::LoadMessageWindow { room, around_seq }) => {
                        load_message_window(&client, &cfg, &shared, &room, around_seq).await;
                        continue;
                    }
                    Some(Action::StartHub) => {
                        // Someone may have started it since the last refresh.
                        if refresh(&client, &cfg, &shared).await.is_ok() {
                            set_notice(true, "the hub is already running".to_string());
                        } else {
                            set_notice(true, "starting the hub...".to_string());
                            let (ok, message) = start_hub(&cfg.server).await;
                            set_notice(ok, message);
                        }
                    }
                    Some(_) => {}
                    None => break,
                }
            }
            _ = tokio::time::sleep(REFRESH_EVERY) => {}
        }
        if let Err(err) = refresh(&client, &cfg, &shared).await {
            let mut snap = shared.lock().expect("snapshot mutex poisoned");
            snap.hub_down = hub_unreachable(&err);
            snap.error = Some(format!("{err:#}"));
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

fn validate_template_id(raw: &str) -> Result<String, String> {
    let id = raw.trim();
    if id.is_empty()
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return Err(
            "Template id must contain only ASCII letters, digits, '-' or '_'".to_string(),
        );
    }
    Ok(id.to_string())
}

fn validate_model_name(raw: &str) -> Result<String, String> {
    let model = raw.trim();
    if model.is_empty()
        || model.len() > 100
        || model
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '{' | '}'))
    {
        return Err(
            "Model must be one word (<= 100 chars) without spaces or braces".to_string(),
        );
    }
    Ok(model.to_string())
}

fn validate_seat_nick(raw: Option<&str>) -> Result<Option<String>, String> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let cleaned = clean_nick(raw);
    if cleaned.is_empty() {
        return Ok(None);
    }
    if cleaned.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("Seat nick must be one nonempty word without spaces".to_string());
    }
    Ok(Some(cleaned.to_string()))
}

fn validate_editor_seat(seat: &GroupEditorSeat, is_lead: bool) -> Result<Value, String> {
    let template = validate_template_id(&seat.template)?;
    let model = validate_model_name(&seat.model)?;
    let count = if is_lead {
        1
    } else if seat.count == 0 {
        return Err("Worker count must be at least 1".to_string());
    } else {
        seat.count
    };
    let nick = validate_seat_nick(seat.nick.as_deref())?;
    let mut obj = json!({
        "template": template,
        "model": model,
        "count": count,
    });
    if let Some(nick) = nick {
        obj["nick"] = json!(nick);
    }
    Ok(obj)
}

impl GroupEditorState {
    pub fn new_group() -> Self {
        Self {
            is_edit: false,
            name: String::new(),
            description: String::new(),
            lead: GroupEditorSeat {
                template: String::new(),
                model: String::new(),
                count: 1,
                nick: None,
            },
            workers: Vec::new(),
            timeout_secs: String::new(),
            review_peers: String::new(),
            review_quorum: String::new(),
            lead_prompt: String::new(),
            worker_prompt: String::new(),
            cursor: 0,
            sub: None,
            error: None,
        }
    }

    pub fn from_group(group: &GroupView) -> Self {
        Self {
            is_edit: true,
            name: group.name.clone(),
            description: group.description.clone().unwrap_or_default(),
            lead: GroupEditorSeat::from(&group.lead),
            workers: group.workers.iter().map(GroupEditorSeat::from).collect(),
            timeout_secs: group
                .inactivity_timeout_secs
                .map(|s| s.to_string())
                .unwrap_or_default(),
            review_peers: group
                .review_policy
                .as_ref()
                .map(|rp| rp.peer_reviewers.join(", "))
                .unwrap_or_default(),
            review_quorum: group
                .review_policy
                .as_ref()
                .and_then(|rp| rp.quorum)
                .map(|q| q.to_string())
                .unwrap_or_default(),
            lead_prompt: group.lead_prompt.clone().unwrap_or_default(),
            worker_prompt: group.worker_prompt.clone().unwrap_or_default(),
            cursor: 1,
            sub: None,
            error: None,
        }
    }

    pub fn fields(&self) -> Vec<GroupEditorField> {
        let mut fields = vec![
            GroupEditorField::Name,
            GroupEditorField::Description,
            GroupEditorField::Lead,
        ];
        for i in 0..self.workers.len() {
            fields.push(GroupEditorField::Worker(i));
        }
        fields.extend([
            GroupEditorField::AddWorker,
            GroupEditorField::TimeoutSecs,
            GroupEditorField::ReviewPeers,
            GroupEditorField::ReviewQuorum,
            GroupEditorField::LeadPrompt,
            GroupEditorField::WorkerPrompt,
            GroupEditorField::Save,
            GroupEditorField::Cancel,
        ]);
        fields
    }

    fn text_value(&self, field: GroupEditorField) -> String {
        match field {
            GroupEditorField::Name => self.name.clone(),
            GroupEditorField::Description => self.description.clone(),
            GroupEditorField::TimeoutSecs => self.timeout_secs.clone(),
            GroupEditorField::ReviewPeers => self.review_peers.clone(),
            GroupEditorField::ReviewQuorum => self.review_quorum.clone(),
            GroupEditorField::LeadPrompt => self.lead_prompt.clone(),
            GroupEditorField::WorkerPrompt => self.worker_prompt.clone(),
            _ => String::new(),
        }
    }

    fn set_text_value(&mut self, field: GroupEditorField, value: String) {
        match field {
            GroupEditorField::Name => self.name = value,
            GroupEditorField::Description => self.description = value,
            GroupEditorField::TimeoutSecs => self.timeout_secs = value,
            GroupEditorField::ReviewPeers => self.review_peers = value,
            GroupEditorField::ReviewQuorum => self.review_quorum = value,
            GroupEditorField::LeadPrompt => self.lead_prompt = value,
            GroupEditorField::WorkerPrompt => self.worker_prompt = value,
            _ => {}
        }
    }

    pub fn build_upsert_payload(&self) -> Result<Value, String> {
        let name = self.name.trim();
        if name.is_empty()
            || matches!(name, "." | "..")
            || name.chars().count() > 100
            || name
                .chars()
                .any(|c| c.is_control() || matches!(c, '/' | '\\'))
        {
            return Err(
                "Group name must be 1..100 characters without '/' or '\\'".to_string(),
            );
        }
        let lead = validate_editor_seat(&self.lead, true)
            .map_err(|err| format!("Lead: {err}"))?;
        if self.workers.is_empty() {
            return Err("A group needs at least one worker".to_string());
        }
        let mut workers = Vec::with_capacity(self.workers.len());
        for (i, w) in self.workers.iter().enumerate() {
            workers.push(
                validate_editor_seat(w, false)
                    .map_err(|err| format!("Worker {}: {err}", i + 1))?,
            );
        }

        let timeout_json = if self.timeout_secs.trim().is_empty() {
            Value::Null
        } else {
            let secs = self
                .timeout_secs
                .trim()
                .parse::<i64>()
                .map_err(|_| "Timeout must be a positive integer (seconds)".to_string())?;
            if secs <= 0 {
                return Err("Timeout must be a positive integer (seconds)".to_string());
            }
            json!(secs)
        };

        let peers: Vec<String> = self
            .review_peers
            .split(',')
            .map(clean_nick)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_ascii_lowercase())
            .collect();
        let review_policy_json = if peers.is_empty() {
            if !self.review_quorum.trim().is_empty() {
                return Err("Review quorum requires at least one peer reviewer".to_string());
            }
            Value::Null
        } else {
            let roster: HashSet<String> = std::iter::once(&self.lead)
                .chain(&self.workers)
                .filter_map(|m| m.nick.as_deref())
                .map(clean_nick)
                .filter(|s| !s.is_empty())
                .map(|s| s.to_ascii_lowercase())
                .collect();
            let mut seen = HashSet::new();
            for peer in &peers {
                if !roster.contains(peer) || !seen.insert(peer.clone()) {
                    return Err(
                        "Peer reviewers must be distinct named members of the group".to_string(),
                    );
                }
            }
            let quorum = if self.review_quorum.trim().is_empty() {
                Value::Null
            } else {
                let q = self
                    .review_quorum
                    .trim()
                    .parse::<u32>()
                    .map_err(|_| "Review quorum must be a positive integer".to_string())?;
                if q == 0 || q as usize > peers.len() {
                    return Err(format!(
                        "Review quorum must be between 1 and {} peer(s)",
                        peers.len()
                    ));
                }
                json!(q)
            };
            json!({
                "peer_reviewers": peers,
                "quorum": quorum,
            })
        };

        for text in [&self.description, &self.lead_prompt, &self.worker_prompt] {
            if text.contains('\0') {
                return Err("Description and prompts must not contain NUL".to_string());
            }
        }

        Ok(json!({
            "name": name,
            "description": optional(&self.description),
            "lead": lead,
            "workers": workers,
            "lead_prompt": optional(&self.lead_prompt),
            "worker_prompt": optional(&self.worker_prompt),
            "review_policy": review_policy_json,
            "inactivity_timeout_secs": timeout_json,
        }))
    }
}

fn template_picker_items(snap: &Snapshot, current: &str) -> Vec<(String, String)> {
    let mut items = Vec::new();
    if !current.is_empty() && !snap.templates.iter().any(|t| t.id == current) {
        items.push((format!("(keep {current})"), current.to_string()));
    }
    for t in &snap.templates {
        let missing = if t.installed { "" } else { "  (not installed)" };
        items.push((format!("{}{missing}", t.name), t.id.clone()));
    }
    items.push(("Type a template id...".to_string(), TYPE_MODEL.to_string()));
    items
}

fn seat_model_items(
    snap: &Snapshot,
    template: &str,
    current: &str,
) -> (Vec<(String, String)>, Option<String>) {
    let mut items = Vec::new();
    if !current.is_empty() {
        items.push((format!("(keep {current})"), current.to_string()));
    }
    let problem = match &snap.models {
        Some((listed_for, Ok(models))) if listed_for == template => {
            for (id, label) in models {
                if id == current {
                    continue;
                }
                let shown = match label {
                    Some(label) => format!("{id}  {label}"),
                    None => id.clone(),
                };
                items.push((shown, id.clone()));
            }
            None
        }
        Some((listed_for, Err(err))) if listed_for == template => Some(err.clone()),
        _ => Some("Asking the hub for models... (or type manually below)".to_string()),
    };
    items.push(("Type a model name...".to_string(), TYPE_MODEL.to_string()));
    (items, problem)
}

fn nick_picker_items(snap: &Snapshot, current: Option<&str>) -> Vec<(String, String)> {
    let mut items = vec![("(pool / unnamed)".to_string(), String::new())];
    if let Some(nick) = current.map(clean_nick).filter(|s| !s.is_empty()) {
        if !snap.accounts.iter().any(|a| nicks_match(&a.nick, nick)) {
            items.push((format!("(keep @{nick})"), nick.to_string()));
        }
    }
    for a in &snap.accounts {
        items.push((format!("@{}  ({})", a.nick, a.role), a.nick.clone()));
    }
    items.push(("+ type account nick...".to_string(), TYPE_MODEL.to_string()));
    items
}

impl GroupSeatEditor {
    fn open(target: Option<usize>, seat: &GroupEditorSeat, snap: &Snapshot) -> Self {
        let wizard = seat.template.is_empty() || seat.model.is_empty();
        let step = if !wizard {
            SeatStep::Menu { row: 0 }
        } else if snap.templates.is_empty() {
            SeatStep::TypeTemplate
        } else {
            SeatStep::PickTemplate { idx: 0 }
        };
        Self {
            target,
            seat: seat.clone(),
            text: if step == SeatStep::TypeTemplate {
                seat.template.clone()
            } else {
                String::new()
            },
            step,
            wizard,
            error: None,
        }
    }

    fn open_new_worker(index: usize, snap: &Snapshot) -> Self {
        let seat = GroupEditorSeat {
            template: String::new(),
            model: String::new(),
            count: 1,
            nick: None,
        };
        Self::open(Some(index), &seat, snap)
    }

    fn menu_len(&self) -> usize {
        if self.target.is_some() {
            5 // Template, Model, Count, Nick, Done
        } else {
            4 // Template, Model, Nick, Done
        }
    }

    fn commit(&self, editor: &mut GroupEditorState) {
        let mut seat = self.seat.clone();
        if self.target.is_none() {
            seat.count = 1;
        }
        match self.target {
            None => editor.lead = seat,
            Some(i) if i < editor.workers.len() => editor.workers[i] = seat,
            Some(_) => {
                editor.workers.push(seat);
                // Keep the cursor on "+ Add worker seat" after inserting the new row above it.
                editor.cursor += 1;
            }
        }
    }
}

fn finish_seat_step(
    editor: &mut GroupEditorState,
    mut seat_ed: GroupSeatEditor,
    just_finished: SeatStep,
    snap: &Snapshot,
) -> (bool, Option<Action>) {
    seat_ed.error = None;
    if !seat_ed.wizard {
        seat_ed.step = SeatStep::Menu { row: 0 };
        editor.sub = Some(GroupEditorSub::Seat(seat_ed));
        return (false, None);
    }
    match just_finished {
        SeatStep::PickTemplate { .. } | SeatStep::TypeTemplate => {
            let tpl = seat_ed.seat.template.clone();
            seat_ed.step = SeatStep::PickModel { idx: 0 };
            editor.sub = Some(GroupEditorSub::Seat(seat_ed));
            (false, Some(Action::LoadModels(tpl)))
        }
        SeatStep::PickModel { .. } | SeatStep::TypeModel => {
            if seat_ed.target.is_some() {
                seat_ed.text = seat_ed.seat.count.to_string();
                seat_ed.step = SeatStep::TypeCount;
            } else {
                seat_ed.step = SeatStep::PickNick { idx: 0 };
            }
            editor.sub = Some(GroupEditorSub::Seat(seat_ed));
            (false, None)
        }
        SeatStep::TypeCount => {
            let _ = snap;
            seat_ed.step = SeatStep::PickNick { idx: 0 };
            editor.sub = Some(GroupEditorSub::Seat(seat_ed));
            (false, None)
        }
        SeatStep::PickNick { .. } | SeatStep::TypeNick => {
            seat_ed.commit(editor);
            editor.sub = None;
            (true, None)
        }
        SeatStep::Menu { .. } => {
            seat_ed.commit(editor);
            editor.sub = None;
            (true, None)
        }
    }
}

fn handle_seat_editor_key(
    editor: &mut GroupEditorState,
    mut seat_ed: GroupSeatEditor,
    code: KeyCode,
    snap: &Snapshot,
) -> Option<Action> {
    let step = seat_ed.step;
    match step {
        SeatStep::Menu { mut row } => {
            let max_row = seat_ed.menu_len() - 1;
            match code {
                KeyCode::Esc => {
                    editor.sub = None;
                    None
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    row = row.saturating_sub(1);
                    seat_ed.step = SeatStep::Menu { row };
                    editor.sub = Some(GroupEditorSub::Seat(seat_ed));
                    None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    row = (row + 1).min(max_row);
                    seat_ed.step = SeatStep::Menu { row };
                    editor.sub = Some(GroupEditorSub::Seat(seat_ed));
                    None
                }
                KeyCode::Char('+') | KeyCode::Char('=') if seat_ed.target.is_some() && row == 2 => {
                    seat_ed.seat.count = seat_ed.seat.count.saturating_add(1);
                    editor.sub = Some(GroupEditorSub::Seat(seat_ed));
                    None
                }
                KeyCode::Char('-') if seat_ed.target.is_some() && row == 2 => {
                    seat_ed.seat.count = seat_ed.seat.count.saturating_sub(1).max(1);
                    editor.sub = Some(GroupEditorSub::Seat(seat_ed));
                    None
                }
                KeyCode::Enter => {
                    let is_worker = seat_ed.target.is_some();
                    let nick_row = if is_worker { 3 } else { 2 };
                    let done_row = if is_worker { 4 } else { 3 };
                    if row == 0 {
                        seat_ed.step = SeatStep::PickTemplate { idx: 0 };
                        editor.sub = Some(GroupEditorSub::Seat(seat_ed));
                        None
                    } else if row == 1 {
                        let tpl = seat_ed.seat.template.clone();
                        seat_ed.step = SeatStep::PickModel { idx: 0 };
                        editor.sub = Some(GroupEditorSub::Seat(seat_ed));
                        (!tpl.is_empty()).then_some(Action::LoadModels(tpl))
                    } else if is_worker && row == 2 {
                        seat_ed.text = seat_ed.seat.count.to_string();
                        seat_ed.step = SeatStep::TypeCount;
                        editor.sub = Some(GroupEditorSub::Seat(seat_ed));
                        None
                    } else if row == nick_row {
                        seat_ed.step = SeatStep::PickNick { idx: 0 };
                        editor.sub = Some(GroupEditorSub::Seat(seat_ed));
                        None
                    } else if row == done_row {
                        seat_ed.commit(editor);
                        editor.sub = None;
                        None
                    } else {
                        editor.sub = Some(GroupEditorSub::Seat(seat_ed));
                        None
                    }
                }
                _ => {
                    editor.sub = Some(GroupEditorSub::Seat(seat_ed));
                    None
                }
            }
        }
        SeatStep::PickTemplate { mut idx } => {
            let items = template_picker_items(snap, &seat_ed.seat.template);
            match code {
                KeyCode::Esc => {
                    if seat_ed.wizard {
                        editor.sub = None;
                    } else {
                        seat_ed.step = SeatStep::Menu { row: 0 };
                        editor.sub = Some(GroupEditorSub::Seat(seat_ed));
                    }
                    None
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    idx = idx.saturating_sub(1);
                    seat_ed.step = SeatStep::PickTemplate { idx };
                    editor.sub = Some(GroupEditorSub::Seat(seat_ed));
                    None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    idx = (idx + 1).min(items.len().saturating_sub(1));
                    seat_ed.step = SeatStep::PickTemplate { idx };
                    editor.sub = Some(GroupEditorSub::Seat(seat_ed));
                    None
                }
                KeyCode::Enter => {
                    let value = items.get(idx).map(|(_, v)| v.clone()).unwrap_or_default();
                    if value == TYPE_MODEL {
                        seat_ed.text = seat_ed.seat.template.clone();
                        seat_ed.step = SeatStep::TypeTemplate;
                        editor.sub = Some(GroupEditorSub::Seat(seat_ed));
                        None
                    } else {
                        seat_ed.seat.template = value;
                        finish_seat_step(editor, seat_ed, step, snap).1
                    }
                }
                _ => {
                    editor.sub = Some(GroupEditorSub::Seat(seat_ed));
                    None
                }
            }
        }
        SeatStep::PickModel { mut idx } => {
            let (items, _) = seat_model_items(snap, &seat_ed.seat.template, &seat_ed.seat.model);
            match code {
                KeyCode::Esc => {
                    if seat_ed.wizard {
                        editor.sub = None;
                    } else {
                        seat_ed.step = SeatStep::Menu { row: 1 };
                        editor.sub = Some(GroupEditorSub::Seat(seat_ed));
                    }
                    None
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    idx = idx.saturating_sub(1);
                    seat_ed.step = SeatStep::PickModel { idx };
                    editor.sub = Some(GroupEditorSub::Seat(seat_ed));
                    None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    idx = (idx + 1).min(items.len().saturating_sub(1));
                    seat_ed.step = SeatStep::PickModel { idx };
                    editor.sub = Some(GroupEditorSub::Seat(seat_ed));
                    None
                }
                KeyCode::Enter => {
                    let value = items.get(idx).map(|(_, v)| v.clone()).unwrap_or_default();
                    if value == TYPE_MODEL {
                        seat_ed.text = seat_ed.seat.model.clone();
                        seat_ed.step = SeatStep::TypeModel;
                        editor.sub = Some(GroupEditorSub::Seat(seat_ed));
                        None
                    } else {
                        seat_ed.seat.model = value;
                        finish_seat_step(editor, seat_ed, step, snap).1
                    }
                }
                _ => {
                    editor.sub = Some(GroupEditorSub::Seat(seat_ed));
                    None
                }
            }
        }
        SeatStep::PickNick { mut idx } => {
            let items = nick_picker_items(snap, seat_ed.seat.nick.as_deref());
            let nick_row = if seat_ed.target.is_some() { 3 } else { 2 };
            match code {
                KeyCode::Esc => {
                    if seat_ed.wizard {
                        editor.sub = None;
                    } else {
                        seat_ed.step = SeatStep::Menu { row: nick_row };
                        editor.sub = Some(GroupEditorSub::Seat(seat_ed));
                    }
                    None
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    idx = idx.saturating_sub(1);
                    seat_ed.step = SeatStep::PickNick { idx };
                    editor.sub = Some(GroupEditorSub::Seat(seat_ed));
                    None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    idx = (idx + 1).min(items.len().saturating_sub(1));
                    seat_ed.step = SeatStep::PickNick { idx };
                    editor.sub = Some(GroupEditorSub::Seat(seat_ed));
                    None
                }
                KeyCode::Enter => {
                    let value = items.get(idx).map(|(_, v)| v.clone()).unwrap_or_default();
                    if value == TYPE_MODEL {
                        seat_ed.text = seat_ed.seat.nick.clone().unwrap_or_default();
                        seat_ed.step = SeatStep::TypeNick;
                        editor.sub = Some(GroupEditorSub::Seat(seat_ed));
                        None
                    } else {
                        seat_ed.seat.nick = if value.is_empty() { None } else { Some(value) };
                        finish_seat_step(editor, seat_ed, step, snap).1
                    }
                }
                _ => {
                    editor.sub = Some(GroupEditorSub::Seat(seat_ed));
                    None
                }
            }
        }
        SeatStep::TypeTemplate | SeatStep::TypeModel | SeatStep::TypeCount | SeatStep::TypeNick => {
            match code {
                KeyCode::Esc => {
                    if seat_ed.wizard {
                        editor.sub = None;
                    } else {
                        let row = match step {
                            SeatStep::TypeTemplate => 0,
                            SeatStep::TypeModel => 1,
                            SeatStep::TypeCount => 2,
                            SeatStep::TypeNick => {
                                if seat_ed.target.is_some() { 3 } else { 2 }
                            }
                            _ => 0,
                        };
                        seat_ed.error = None;
                        seat_ed.step = SeatStep::Menu { row };
                        editor.sub = Some(GroupEditorSub::Seat(seat_ed));
                    }
                    None
                }
                KeyCode::Backspace => {
                    seat_ed.text.pop();
                    seat_ed.error = None;
                    editor.sub = Some(GroupEditorSub::Seat(seat_ed));
                    None
                }
                KeyCode::Char(c) => {
                    seat_ed.text.push(c);
                    seat_ed.error = None;
                    editor.sub = Some(GroupEditorSub::Seat(seat_ed));
                    None
                }
                KeyCode::Enter => match step {
                    SeatStep::TypeTemplate => match validate_template_id(&seat_ed.text) {
                        Ok(tpl) => {
                            seat_ed.seat.template = tpl;
                            finish_seat_step(editor, seat_ed, step, snap).1
                        }
                        Err(err) => {
                            seat_ed.error = Some(err);
                            editor.sub = Some(GroupEditorSub::Seat(seat_ed));
                            None
                        }
                    },
                    SeatStep::TypeModel => match validate_model_name(&seat_ed.text) {
                        Ok(model) => {
                            seat_ed.seat.model = model;
                            finish_seat_step(editor, seat_ed, step, snap).1
                        }
                        Err(err) => {
                            seat_ed.error = Some(err);
                            editor.sub = Some(GroupEditorSub::Seat(seat_ed));
                            None
                        }
                    },
                    SeatStep::TypeCount => {
                        match seat_ed.text.trim().parse::<u32>().ok().filter(|&c| c >= 1) {
                            Some(count) => {
                                seat_ed.seat.count = count;
                                finish_seat_step(editor, seat_ed, step, snap).1
                            }
                            None => {
                                seat_ed.error =
                                    Some("Worker count must be an integer >= 1".to_string());
                                editor.sub = Some(GroupEditorSub::Seat(seat_ed));
                                None
                            }
                        }
                    }
                    SeatStep::TypeNick => {
                        let raw = seat_ed.text.trim();
                        let maybe = (!raw.is_empty()).then_some(raw);
                        match validate_seat_nick(maybe) {
                            Ok(nick) => {
                                seat_ed.seat.nick = nick;
                                finish_seat_step(editor, seat_ed, step, snap).1
                            }
                            Err(err) => {
                                seat_ed.error = Some(err);
                                editor.sub = Some(GroupEditorSub::Seat(seat_ed));
                                None
                            }
                        }
                    }
                    _ => None,
                },
                _ => {
                    editor.sub = Some(GroupEditorSub::Seat(seat_ed));
                    None
                }
            }
        }
    }
}

fn submit_group_editor(app: &mut App, mut editor: Box<GroupEditorState>) -> Option<Action> {
    match editor.build_upsert_payload() {
        Ok(body) => {
            let name = editor.name.trim().to_string();
            let done = if editor.is_edit {
                format!("Group '{name}' updated")
            } else {
                format!("Group '{name}' created")
            };
            Some(Action::Api {
                path: "/v1/groups".to_string(),
                body,
                owner: true,
                done,
            })
        }
        Err(err) => {
            editor.error = Some(err);
            app.modal = Some(Modal::GroupEditor(editor));
            None
        }
    }
}

fn handle_group_editor_key(
    app: &mut App,
    mut editor: Box<GroupEditorState>,
    code: KeyCode,
    modifiers: KeyModifiers,
    snap: &Snapshot,
) -> Option<Action> {
    if let Some(sub) = editor.sub.take() {
        let action = match sub {
            GroupEditorSub::Text { field, mut text } => match code {
                KeyCode::Esc => None,
                KeyCode::Enter => {
                    editor.set_text_value(field, text);
                    editor.error = None;
                    None
                }
                KeyCode::Backspace => {
                    text.pop();
                    editor.sub = Some(GroupEditorSub::Text { field, text });
                    None
                }
                KeyCode::Char(c) => {
                    text.push(c);
                    editor.sub = Some(GroupEditorSub::Text { field, text });
                    None
                }
                _ => {
                    editor.sub = Some(GroupEditorSub::Text { field, text });
                    None
                }
            },
            GroupEditorSub::Seat(seat_ed) => handle_seat_editor_key(&mut editor, seat_ed, code, snap),
        };
        app.modal = Some(Modal::GroupEditor(editor));
        return action;
    }

    if modifiers.contains(KeyModifiers::CONTROL) && matches!(code, KeyCode::Char('s') | KeyCode::Char('S')) {
        return submit_group_editor(app, editor);
    }

    let fields = editor.fields();
    editor.cursor = editor.cursor.min(fields.len().saturating_sub(1));
    let current = fields[editor.cursor];

    match code {
        KeyCode::Esc => None,
        KeyCode::Char('s') => submit_group_editor(app, editor),
        KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => {
            editor.cursor = editor.cursor.saturating_sub(1);
            editor.error = None;
            app.modal = Some(Modal::GroupEditor(editor));
            None
        }
        KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
            editor.cursor = (editor.cursor + 1).min(fields.len().saturating_sub(1));
            editor.error = None;
            app.modal = Some(Modal::GroupEditor(editor));
            None
        }
        KeyCode::Char('+') | KeyCode::Char('=') => {
            if let GroupEditorField::Worker(i) = current {
                editor.workers[i].count = editor.workers[i].count.saturating_add(1);
                editor.error = None;
            }
            app.modal = Some(Modal::GroupEditor(editor));
            None
        }
        KeyCode::Char('-') => {
            if let GroupEditorField::Worker(i) = current {
                editor.workers[i].count = editor.workers[i].count.saturating_sub(1).max(1);
                editor.error = None;
            }
            app.modal = Some(Modal::GroupEditor(editor));
            None
        }
        KeyCode::Char('d') | KeyCode::Delete => {
            if let GroupEditorField::Worker(i) = current {
                if editor.workers.len() > 1 {
                    editor.workers.remove(i);
                    let new_len = editor.fields().len();
                    editor.cursor = editor.cursor.min(new_len.saturating_sub(1));
                    editor.error = None;
                } else {
                    editor.error = Some("A group needs at least one worker".to_string());
                }
            }
            app.modal = Some(Modal::GroupEditor(editor));
            None
        }
        KeyCode::Enter => match current {
            GroupEditorField::Name if editor.is_edit => {
                editor.error = Some("Group name cannot be renamed when editing".to_string());
                app.modal = Some(Modal::GroupEditor(editor));
                None
            }
            GroupEditorField::Name
            | GroupEditorField::Description
            | GroupEditorField::TimeoutSecs
            | GroupEditorField::ReviewPeers
            | GroupEditorField::ReviewQuorum
            | GroupEditorField::LeadPrompt
            | GroupEditorField::WorkerPrompt => {
                let text = editor.text_value(current);
                editor.sub = Some(GroupEditorSub::Text {
                    field: current,
                    text,
                });
                editor.error = None;
                app.modal = Some(Modal::GroupEditor(editor));
                None
            }
            GroupEditorField::Lead => {
                let seat_ed = GroupSeatEditor::open(None, &editor.lead, snap);
                editor.sub = Some(GroupEditorSub::Seat(seat_ed));
                editor.error = None;
                app.modal = Some(Modal::GroupEditor(editor));
                None
            }
            GroupEditorField::Worker(i) => {
                let seat_ed = GroupSeatEditor::open(Some(i), &editor.workers[i], snap);
                editor.sub = Some(GroupEditorSub::Seat(seat_ed));
                editor.error = None;
                app.modal = Some(Modal::GroupEditor(editor));
                None
            }
            GroupEditorField::AddWorker => {
                let seat_ed = GroupSeatEditor::open_new_worker(editor.workers.len(), snap);
                editor.sub = Some(GroupEditorSub::Seat(seat_ed));
                editor.error = None;
                app.modal = Some(Modal::GroupEditor(editor));
                None
            }
            GroupEditorField::Save => submit_group_editor(app, editor),
            GroupEditorField::Cancel => None,
        },
        _ => {
            app.modal = Some(Modal::GroupEditor(editor));
            None
        }
    }
}

fn handle_group_attach_key(
    app: &mut App,
    group_name: String,
    attached: Vec<String>,
    mut selected: Vec<String>,
    mut idx: usize,
    code: KeyCode,
    snap: &Snapshot,
) -> Option<Action> {
    if snap.projects.is_empty() {
        if code != KeyCode::Esc {
            app.modal = Some(Modal::GroupAttach {
                group_name,
                attached,
                selected,
                idx: 0,
            });
        }
        return None;
    }
    let last = snap.projects.len().saturating_sub(1);
    idx = idx.min(last);
    match code {
        KeyCode::Esc => None,
        KeyCode::Up | KeyCode::Char('k') => {
            idx = idx.saturating_sub(1);
            app.modal = Some(Modal::GroupAttach {
                group_name,
                attached,
                selected,
                idx,
            });
            None
        }
        KeyCode::Down | KeyCode::Char('j') => {
            idx = (idx + 1).min(last);
            app.modal = Some(Modal::GroupAttach {
                group_name,
                attached,
                selected,
                idx,
            });
            None
        }
        KeyCode::Char(' ') => {
            let channel = &snap.projects[idx].channel;
            if !attached.iter().any(|c| c == channel) {
                if let Some(pos) = selected.iter().position(|c| c == channel) {
                    selected.remove(pos);
                } else {
                    selected.push(channel.clone());
                }
            }
            app.modal = Some(Modal::GroupAttach {
                group_name,
                attached,
                selected,
                idx,
            });
            None
        }
        KeyCode::Enter => {
            let targets: Vec<String> = if selected.is_empty() {
                vec![snap.projects[idx].channel.clone()]
            } else {
                selected
            };
            let encoded = encode_path_segment(&group_name);
            let path = format!("/v1/groups/{encoded}/attach");
            if targets.len() == 1 {
                let channel = &targets[0];
                Some(Action::Api {
                    path,
                    body: json!({"channel": channel}),
                    owner: true,
                    done: format!("Attached group '{group_name}' to {channel}"),
                })
            } else {
                let steps = targets
                    .iter()
                    .map(|channel| Action::Api {
                        path: path.clone(),
                        body: json!({"channel": channel}),
                        owner: true,
                        done: format!("Attached group '{group_name}' to {} project(s)", targets.len()),
                    })
                    .collect();
                Some(Action::Many(steps))
            }
        }
        _ => {
            app.modal = Some(Modal::GroupAttach {
                group_name,
                attached,
                selected,
                idx,
            });
            None
        }
    }
}

fn search_room_matches_scope(
    scope: SearchScope,
    current_room: Option<&str>,
    room: &str,
    is_sprint: bool,
    is_dm: bool,
) -> bool {
    match scope {
        SearchScope::All => true,
        SearchScope::Sprints => is_sprint,
        SearchScope::DirectMessages => is_dm,
        SearchScope::CurrentRoom => current_room == Some(room),
    }
}

fn search_query_matches(parsed: &ParsedSearchQuery, room: &str, sender: &str, body: &str) -> bool {
    let room_l = room.to_ascii_lowercase();
    let sender_l = sender.to_ascii_lowercase();
    let body_l = body.to_ascii_lowercase();
    parsed.from_filters.iter().all(|f| sender_l.contains(f))
        && parsed.in_filters.iter().all(|r| room_l.contains(r))
        && parsed
            .terms
            .iter()
            .all(|t| body_l.contains(t) || sender_l.contains(t))
}

/// Search loaded chat history (and any matching server search response) for `state`.
pub fn search_hits(state: &SearchState, snap: &Snapshot) -> Vec<SearchHit> {
    let parsed = ParsedSearchQuery::parse(&state.query);
    if parsed.is_empty() {
        return Vec::new();
    }
    let mut hits: Vec<SearchHit> = Vec::new();
    let mut seen: HashSet<(String, i64)> = HashSet::new();

    for (room, lines) in &snap.messages {
        let is_dm = room.starts_with('@');
        let is_sprint = !is_dm && project_for(snap, room).is_some_and(|p| p.channel != *room);
        if !search_room_matches_scope(
            state.scope,
            state.current_room.as_deref(),
            room,
            is_sprint,
            is_dm,
        ) {
            continue;
        }
        for line in lines {
            if search_query_matches(&parsed, room, &line.sender, &line.body)
                && seen.insert((room.clone(), line.seq))
            {
                hits.push(SearchHit {
                    room: room.clone(),
                    seq: line.seq,
                    sender: line.sender.clone(),
                    body: line.body.clone(),
                    time: line.time,
                    is_sprint,
                    is_dm,
                });
            }
        }
    }

    if let Some(resp) = &snap.search_results {
        let scoped_channel = if state.scope == SearchScope::CurrentRoom {
            state.current_room.as_deref()
        } else {
            None
        };
        if resp.query.trim() == state.query.trim()
            && resp.scope == state.scope
            && resp.channel.as_deref() == scoped_channel
        {
            for hit in &resp.hits {
                if search_room_matches_scope(
                    state.scope,
                    state.current_room.as_deref(),
                    &hit.room,
                    hit.is_sprint,
                    hit.is_dm,
                ) && search_query_matches(&parsed, &hit.room, &hit.sender, &hit.body)
                    && seen.insert((hit.room.clone(), hit.seq))
                {
                    hits.push(hit.clone());
                }
            }
        }
    }

    hits.sort_by(|a, b| b.seq.cmp(&a.seq).then_with(|| a.room.cmp(&b.room)));
    hits
}

fn search_action_for(state: &SearchState) -> Option<Action> {
    let parsed = ParsedSearchQuery::parse(&state.query);
    if parsed.is_empty() {
        return None;
    }
    Some(Action::SearchHistory {
        query: state.query.trim().to_string(),
        scope: state.scope,
        channel: if state.scope == SearchScope::CurrentRoom {
            state.current_room.clone()
        } else {
            None
        },
    })
}

fn open_search_modal(app: &mut App, snap: &Snapshot) {
    let current_room = if app.view == View::Chat {
        chat_rooms(app, snap).get(app.channel_idx).cloned()
    } else {
        app.project.clone()
    };
    app.modal = Some(Modal::Search(Box::new(SearchState {
        query: String::new(),
        scope: SearchScope::All,
        current_room,
        idx: 0,
    })));
}

fn handle_search_key(
    app: &mut App,
    mut state: Box<SearchState>,
    code: KeyCode,
    modifiers: KeyModifiers,
    snap: &Snapshot,
) -> Option<Action> {
    let has_room = state.current_room.is_some();
    match code {
        KeyCode::Esc => None,
        KeyCode::Tab => {
            state.scope = state.scope.next(has_room);
            state.idx = 0;
            let action = search_action_for(&state);
            app.modal = Some(Modal::Search(state));
            action
        }
        KeyCode::BackTab => {
            state.scope = state.scope.prev(has_room);
            state.idx = 0;
            let action = search_action_for(&state);
            app.modal = Some(Modal::Search(state));
            action
        }
        KeyCode::Up => {
            state.idx = state.idx.saturating_sub(1);
            app.modal = Some(Modal::Search(state));
            None
        }
        KeyCode::Down => {
            let len = search_hits(&state, snap).len();
            state.idx = (state.idx + 1).min(len.saturating_sub(1));
            app.modal = Some(Modal::Search(state));
            None
        }
        KeyCode::PageUp => {
            state.idx = state.idx.saturating_sub(10);
            app.modal = Some(Modal::Search(state));
            None
        }
        KeyCode::PageDown => {
            let len = search_hits(&state, snap).len();
            state.idx = (state.idx + 10).min(len.saturating_sub(1));
            app.modal = Some(Modal::Search(state));
            None
        }
        KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => {
            state.query.clear();
            state.idx = 0;
            app.modal = Some(Modal::Search(state));
            None
        }
        KeyCode::Backspace => {
            state.query.pop();
            state.idx = 0;
            let action = search_action_for(&state);
            app.modal = Some(Modal::Search(state));
            action
        }
        KeyCode::Char(c)
            if !modifiers.contains(KeyModifiers::CONTROL) && !modifiers.contains(KeyModifiers::ALT) =>
        {
            state.query.push(c);
            state.idx = 0;
            let action = search_action_for(&state);
            app.modal = Some(Modal::Search(state));
            action
        }
        KeyCode::Enter => {
            let hits = search_hits(&state, snap);
            if let Some(hit) = hits.get(state.idx.min(hits.len().saturating_sub(1))) {
                if hit.is_dm && !app.dm_open.contains(&hit.room) {
                    app.dm_open.push(hit.room.clone());
                }
                app.chat_channel = Some(hit.room.clone());
                app.chat_scroll = 0;
                app.jump_to_seq = Some((hit.room.clone(), hit.seq));
                app.chat_highlight = Some((hit.room.clone(), hit.seq));
                app.view = View::Chat;
                let has_local = snap
                    .messages
                    .get(&hit.room)
                    .is_some_and(|lines| lines.iter().any(|l| l.seq == hit.seq));
                if !has_local {
                    return Some(Action::LoadMessageWindow {
                        room: hit.room.clone(),
                        around_seq: hit.seq,
                    });
                }
                return None;
            }
            let action = search_action_for(&state);
            app.modal = Some(Modal::Search(state));
            action
        }
        _ => {
            app.modal = Some(Modal::Search(state));
            None
        }
    }
}

fn handle_modal(
    app: &mut App,
    code: KeyCode,
    modifiers: KeyModifiers,
    snap: &Snapshot,
) -> Option<Action> {
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
        Modal::GroupEditor(editor) => handle_group_editor_key(app, editor, code, modifiers, snap),
        Modal::GroupAttach {
            group_name,
            attached,
            selected,
            idx,
        } => handle_group_attach_key(app, group_name, attached, selected, idx, code, snap),
        Modal::Search(state) => handle_search_key(app, state, code, modifiers, snap),
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
        return handle_modal(app, code, modifiers, snap);
    }
    // S starts a hub that is not running; Ctrl+S too, so chat can type an S.
    let start_key = match code {
        KeyCode::Char('s') | KeyCode::Char('S') if modifiers.contains(KeyModifiers::CONTROL) => {
            true
        }
        KeyCode::Char('S') => app.view != View::Chat,
        _ => false,
    };
    if start_key && snap.hub_down && hub_bind_for(&snap.server).is_some() {
        return Some(Action::StartHub);
    }
    if modifiers.contains(KeyModifiers::CONTROL) && matches!(code, KeyCode::Char('f')) {
        open_search_modal(app, snap);
        return None;
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
        View::Groups => handle_groups_key(app, code, snap),
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
        KeyCode::Char('g') | KeyCode::Char('G') => {
            select_project_group(app, snap);
            app.view = View::Groups;
        }
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
        KeyCode::Char('/') => open_search_modal(app, snap),
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

fn handle_groups_key(app: &mut App, code: KeyCode, snap: &Snapshot) -> Option<Action> {
    let group_last = snap.groups.len().saturating_sub(1);
    app.group_idx = app.group_idx.min(group_last);
    let subtasks = snap
        .groups
        .get(app.group_idx)
        .map(|g| group_subtasks(snap, g))
        .unwrap_or_default();
    let subtask_last = subtasks.len().saturating_sub(1);
    app.group_subtask_cursor = app.group_subtask_cursor.min(subtask_last);

    match code {
        KeyCode::Char('q') | KeyCode::Char('Q') => return Some(Action::Quit),
        KeyCode::Esc | KeyCode::Char('d') | KeyCode::Char('D') => app.view = View::Dashboard,
        KeyCode::Char('c') | KeyCode::Char('C') => {
            let room = snap
                .groups
                .get(app.group_idx)
                .and_then(|g| g.projects.first().cloned())
                .or_else(|| app.project.clone());
            if let Some(index) = room.and_then(|r| snap.channels.iter().position(|c| *c == r)) {
                app.channel_idx = index;
                app.chat_channel = None;
            }
            app.chat_scroll = 0;
            app.view = View::Chat;
        }
        KeyCode::Char('r') | KeyCode::Char('R') => return Some(Action::Refresh),
        KeyCode::Char('/') => open_search_modal(app, snap),
        KeyCode::Char('?') | KeyCode::Char('h') => app.modal = Some(Modal::Help),
        KeyCode::Char('n') | KeyCode::Char('N') => {
            app.modal = Some(Modal::GroupEditor(Box::new(GroupEditorState::new_group())));
        }
        KeyCode::Char('e') | KeyCode::Char('E') => {
            if let Some(group) = snap.groups.get(app.group_idx) {
                app.modal = Some(Modal::GroupEditor(Box::new(GroupEditorState::from_group(
                    group,
                ))));
            }
        }
        KeyCode::Char('a') | KeyCode::Char('A') => {
            if let Some(group) = snap.groups.get(app.group_idx) {
                let idx = app
                    .project
                    .as_deref()
                    .and_then(|ch| snap.projects.iter().position(|p| p.channel == ch))
                    .unwrap_or(0);
                app.modal = Some(Modal::GroupAttach {
                    group_name: group.name.clone(),
                    attached: group.projects.clone(),
                    selected: Vec::new(),
                    idx,
                });
            }
        }
        KeyCode::Tab | KeyCode::BackTab => {
            app.groups_focus = match app.groups_focus {
                GroupsFocus::Groups => GroupsFocus::Subtasks,
                GroupsFocus::Subtasks => GroupsFocus::Groups,
            };
        }
        KeyCode::Right => app.groups_focus = GroupsFocus::Subtasks,
        KeyCode::Left => app.groups_focus = GroupsFocus::Groups,
        KeyCode::Down | KeyCode::Char('j') => match app.groups_focus {
            GroupsFocus::Groups => {
                let next = (app.group_idx + 1).min(group_last);
                if next != app.group_idx {
                    app.group_idx = next;
                    app.group_subtask_cursor = 0;
                    app.group_subtask_scroll = 0;
                }
            }
            GroupsFocus::Subtasks => {
                app.group_subtask_cursor = (app.group_subtask_cursor + 1).min(subtask_last);
            }
        },
        KeyCode::Up | KeyCode::Char('k') => match app.groups_focus {
            GroupsFocus::Groups => {
                let next = app.group_idx.saturating_sub(1);
                if next != app.group_idx {
                    app.group_idx = next;
                    app.group_subtask_cursor = 0;
                    app.group_subtask_scroll = 0;
                }
            }
            GroupsFocus::Subtasks => {
                app.group_subtask_cursor = app.group_subtask_cursor.saturating_sub(1);
            }
        },
        KeyCode::PageDown => match app.groups_focus {
            GroupsFocus::Groups => {
                let next = (app.group_idx + 10).min(group_last);
                if next != app.group_idx {
                    app.group_idx = next;
                    app.group_subtask_cursor = 0;
                    app.group_subtask_scroll = 0;
                }
            }
            GroupsFocus::Subtasks => {
                app.group_subtask_cursor = (app.group_subtask_cursor + 10).min(subtask_last);
            }
        },
        KeyCode::PageUp => match app.groups_focus {
            GroupsFocus::Groups => {
                let next = app.group_idx.saturating_sub(10);
                if next != app.group_idx {
                    app.group_idx = next;
                    app.group_subtask_cursor = 0;
                    app.group_subtask_scroll = 0;
                }
            }
            GroupsFocus::Subtasks => {
                app.group_subtask_cursor = app.group_subtask_cursor.saturating_sub(10);
            }
        },
        KeyCode::Enter => match app.groups_focus {
            GroupsFocus::Groups if !subtasks.is_empty() => {
                app.groups_focus = GroupsFocus::Subtasks;
            }
            GroupsFocus::Subtasks => {
                if let Some(sub) = subtasks.get(app.group_subtask_cursor) {
                    app.modal = Some(Modal::Detail {
                        task_id: sub.task_id.clone(),
                    });
                    return Some(Action::LoadTask(sub.task_id.clone()));
                }
            }
            _ => {}
        },
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
            app.jump_to_seq = None;
            app.chat_highlight = None;
        }
        KeyCode::Down => {
            if app.channel_idx + 1 < chat_rooms(app, snap).len() {
                app.channel_idx += 1;
            }
            app.chat_channel = None;
            app.chat_scroll = 0;
            app.jump_to_seq = None;
            app.chat_highlight = None;
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
        KeyCode::End => {
            app.chat_scroll = 0;
            app.jump_to_seq = None;
            app.chat_highlight = None;
        }
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
            app.jump_to_seq = None;
            app.chat_highlight = None;
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
        } else if within("[G]roups") {
            app.view = View::Groups;
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
                app.jump_to_seq = None;
                app.chat_highlight = None;
            }
        }
        View::Chat if app.people_area.contains((column, row).into()) => {
            if let Some((_, nick)) = app.people_rows.iter().find(|(y, _)| *y == row) {
                let nick = nick.clone();
                open_dm(app, &nick);
            }
        }
        View::Groups => {
            if column < GROUPS_PANE_WIDTH {
                if let Some((_, index)) = app.group_rows.iter().find(|(y, _)| *y == row) {
                    if app.group_idx != *index {
                        app.group_idx = *index;
                        app.group_subtask_cursor = 0;
                        app.group_subtask_scroll = 0;
                    }
                    app.groups_focus = GroupsFocus::Groups;
                }
            } else if let Some((_, index)) = app.group_subtask_rows.iter().find(|(y, _)| *y == row) {
                app.group_subtask_cursor = *index;
                app.groups_focus = GroupsFocus::Subtasks;
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
        "review" | "super_review" => Color::Blue,
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
        View::Groups => draw_groups(frame, app, snap, body),
        View::Chat => draw_chat(frame, app, snap, body),
    }

    let who = format!(
        "  you: @{} ({})  CAMS  {}",
        snap.me,
        if snap.my_role.is_empty() {
            "?"
        } else {
            &snap.my_role
        },
        snap.server,
    );
    let mut header_spans = tab_spans(app.view);
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
    header_spans.push(Span::raw(who));
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
        View::Groups => &[
            ("n", "new"),
            ("e", "edit"),
            ("a", "attach"),
            ("Tab", "pane"),
            ("Enter", "task"),
            ("d", "dashboard"),
            ("c", "chat"),
            ("?", "help"),
            ("q", "quit"),
        ],
        View::Chat => &[
            ("Enter", "send"),
            ("Tab", "complete @name"),
            ("Up/Down", "room"),
            ("^D", "direct message"),
            ("^F", "search"),
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
        (Some(_), None) if snap.hub_down => hub_down_line(snap, app.view),
        (Some(_), Some((ok, message, _))) if snap.hub_down => Line::from(Span::styled(
            format!(" {message}"),
            bold().fg(if *ok { Color::Green } else { Color::Red }),
        )),
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

/// The footer while nothing answers at the hub's address: what is wrong
/// and, for a hub on this machine, the key that starts it.
fn hub_down_line(snap: &Snapshot, view: View) -> Line<'static> {
    let mut spans = vec![Span::styled(
        format!(" No hub is running at {}. ", snap.server),
        bold().fg(Color::Red),
    )];
    if hub_bind_for(&snap.server).is_some() {
        let key = if view == View::Chat { "^S" } else { "S" };
        spans.push(Span::styled(
            format!("[{key}]"),
            bold().fg(Color::Black).bg(Color::Green),
        ));
        spans.push(Span::raw(" start it "));
    } else {
        spans.push(Span::raw("Start it on its machine. "));
    }
    let quit = if view == View::Chat { "[^C]" } else { "[q]" };
    spans.push(Span::styled(quit, bold().fg(Color::Black).bg(Color::Red)));
    spans.push(Span::raw(" quit"));
    Line::from(spans)
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
    if lines.is_empty() && snap.hub_down {
        let text = if hub_bind_for(&snap.server).is_some() {
            "The hub is not running. Press S to start it on this machine."
        } else {
            "The hub cannot be reached. Start it on its machine."
        };
        lines.push(Line::from(Span::styled(text, bold().fg(Color::Red))));
    } else if lines.is_empty() {
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

/// The background behind a key hint, by what the key does: moving around
/// is blue, making or changing something green, talking yellow, waking an
/// agent (which spends its quota) magenta, quitting red, help grey.
pub fn hint_color(key: &str, label: &str) -> Color {
    match (key, label) {
        ("q", _) | ("Q", _) => Color::Red,
        ("?", _) => Color::Gray,
        ("t", _) => Color::Magenta,
        (_, "answer" | "chat" | "send" | "complete @name" | "direct message" | "search") => {
            Color::Yellow
        }
        ("Enter" | "Esc" | "Up/Down" | "PgUp/PgDn" | "End", _) => Color::Blue,
        _ => Color::Green,
    }
}

fn hint_line(hints: &[(&str, &str)]) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (key, label) in hints {
        spans.push(Span::styled(
            format!("[{key}]"),
            bold().fg(Color::Black).bg(hint_color(key, label)),
        ));
        spans.push(Span::raw(format!(" {label} ")));
    }
    Line::from(spans)
}

/// The tab bar: each tab's bracketed letter on a coloured background and
/// its label after it, the open view's label highlighted. The text is
/// exactly `TABS`, so clicks can be found by position in it.
fn tab_spans(view: View) -> Vec<Span<'static>> {
    let tabs = [
        ("[H]", "ome", Some(View::Home)),
        ("[D]", "ashboard", Some(View::Dashboard)),
        ("[G]", "roups", Some(View::Groups)),
        ("[C]", "hat", Some(View::Chat)),
        ("[Q]", "uit", None),
    ];
    let mut spans = vec![Span::raw(" ")];
    for (at, (key, label, tab)) in tabs.into_iter().enumerate() {
        let open = tab == Some(view);
        let key_color = match tab {
            None => Color::Red,
            Some(_) if open => Color::Yellow,
            Some(_) => Color::Cyan,
        };
        spans.push(Span::styled(key, bold().fg(Color::Black).bg(key_color)));
        spans.push(Span::styled(
            label,
            if open {
                bold().fg(Color::Black).bg(Color::Yellow)
            } else {
                Style::default()
            },
        ));
        spans.push(Span::raw(if at == tabs.len() - 1 { " " } else { "  " }));
    }
    spans
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
            let state = task_activity_rank(snap, task);
            if best.is_none_or(|current| state.0 < current.0) {
                best = Some(state);
            }
        }
    }
    // A running agent is working, even on a task it has not moved yet.
    let running = snap
        .accounts
        .iter()
        .any(|a| a.nick.eq_ignore_ascii_case(nick) && a.running);
    if running && best.is_none_or(|(rank, _, _)| rank > 4) {
        return Some((Color::Green, "working"));
    }
    best.map(|(_, color, label)| (color, label))
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
                let priority_span = match task.priority.as_str() {
                    "urgent" => Span::styled(
                        "[urgent] ",
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    ),
                    "high" => Span::styled("[high] ", Style::default().fg(Color::Yellow)),
                    "low" => Span::styled("[low] ", dim()),
                    _ => Span::raw(""),
                };
                Line::from(vec![
                    Span::raw("  "),
                    Span::styled(
                        format!("{:<18}", task.status),
                        Style::default().fg(status_color(&task.status)),
                    ),
                    Span::styled(format!("@{:<14}", task.assignee), bold()),
                    Span::styled(format!("{sprint:<4}"), Style::default().fg(Color::Magenta)),
                    Span::styled(branch, dim()),
                    priority_span,
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

fn short_id(id: &str) -> String {
    id.chars().take(13).collect()
}

fn priority_span(priority: &str) -> Span<'static> {
    match priority {
        "urgent" => Span::styled(
            "[urgent] ",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        "high" => Span::styled("[high] ", Style::default().fg(Color::Yellow)),
        "low" => Span::styled("[low] ", dim()),
        _ => Span::raw(""),
    }
}

fn seat_spec(seat: &GroupSeatView) -> String {
    match (seat.template.is_empty(), seat.model.is_empty()) {
        (true, true) => "-".to_string(),
        (false, true) => seat.template.clone(),
        (true, false) => seat.model.clone(),
        (false, false) => format!("{}:{}", seat.template, seat.model),
    }
}

fn group_topology_lines(
    snap: &Snapshot,
    group: &GroupView,
    subtasks: &[GroupSubtaskItem],
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let attached = if group.projects.is_empty() {
        "(unattached)".to_string()
    } else {
        group.projects.join(", ")
    };
    let mut meta = Vec::new();
    if let Some(desc) = &group.description {
        meta.push(Span::raw(format!("{desc}  ·  ")));
    }
    meta.push(Span::styled("projects: ", dim()));
    meta.push(Span::styled(
        attached,
        if group.projects.is_empty() {
            dim()
        } else {
            Style::default().fg(Color::Cyan)
        },
    ));
    lines.push(Line::from(meta));

    if group.review_policy.is_some() || group.inactivity_timeout_secs.is_some() {
        let mut policy_spans = Vec::new();
        if let Some(rp) = &group.review_policy {
            let quorum = match rp.quorum {
                Some(q) => format!("quorum {q}"),
                None => "all peers".to_string(),
            };
            let peers = rp
                .peer_reviewers
                .iter()
                .map(|p| format!("@{p}"))
                .collect::<Vec<_>>()
                .join(", ");
            policy_spans.push(Span::styled(
                format!("policy: {quorum} ({peers})"),
                dim(),
            ));
        }
        if let Some(secs) = group.inactivity_timeout_secs {
            if !policy_spans.is_empty() {
                policy_spans.push(Span::styled("  ·  ", dim()));
            }
            policy_spans.push(Span::styled(format!("timeout: {secs}s"), dim()));
        }
        lines.push(Line::from(policy_spans));
    }

    // Lead node at the root of the group topology.
    let lead_spec = seat_spec(&group.lead);
    let lead_tasks = match group.lead.nick.as_deref() {
        Some(nick) => group_member_tasks(snap, group, nick),
        None => Vec::new(),
    };
    let mut shown_task_ids: HashSet<&str> = HashSet::new();
    match group.lead.nick.as_deref() {
        Some(nick) => {
            let state = group_member_state(snap, group, nick);
            lines.push(Line::from(vec![
                Span::styled("★ ", Style::default().fg(Color::Yellow)),
                Span::styled(format!("@{nick}"), bold()),
                Span::styled(format!("  ({lead_spec})  "), dim()),
                Span::styled(state.label, Style::default().fg(state.color)),
            ]));
            let cont = if group.workers.is_empty() && subtasks.is_empty() {
                "   "
            } else {
                "│  "
            };
            for task in &lead_tasks {
                shown_task_ids.insert(task.id.as_str());
                let role_tag = if task.parent.is_some() {
                    "[subtask]"
                } else {
                    "[parent]"
                };
                lines.push(Line::from(vec![
                    Span::styled(format!("{cont}◆ "), dim()),
                    Span::styled(format!("{role_tag} "), Style::default().fg(Color::Cyan)),
                    Span::styled(
                        format!("{} ", task.status),
                        Style::default().fg(status_color(&task.status)),
                    ),
                    Span::raw(task.title.clone()),
                    Span::styled(format!("  {}", short_id(&task.id)), dim()),
                ]));
            }
        }
        None => {
            lines.push(Line::from(vec![
                Span::styled("★ ", Style::default().fg(Color::Yellow)),
                Span::styled("(unnamed lead)", dim()),
                Span::styled(format!("  ({lead_spec})"), dim()),
            ]));
        }
    }

    // Collect subtasks not assigned to any named worker seat (e.g., unassigned
    // or assigned to a non-listed seat) so nothing is hidden from the topology.
    let worker_nicks: Vec<&str> = group
        .workers
        .iter()
        .filter_map(|w| w.nick.as_deref())
        .collect();
    let other_subtasks: Vec<&GroupSubtaskItem> = subtasks
        .iter()
        .filter(|sub| {
            !shown_task_ids.contains(sub.task_id.as_str())
                && !worker_nicks.iter().any(|w| nicks_match(w, &sub.assignee))
        })
        .collect();

    for (i, worker) in group.workers.iter().enumerate() {
        let has_more = i + 1 < group.workers.len() || !other_subtasks.is_empty();
        let branch = if has_more { "├─ " } else { "└─ " };
        let cont = if has_more { "│  " } else { "   " };
        let spec = seat_spec(worker);
        let count_suffix = if worker.count > 1 {
            format!(" ×{}", worker.count)
        } else {
            String::new()
        };

        match worker.nick.as_deref() {
            Some(nick) => {
                let state = group_member_state(snap, group, nick);
                let dot = if state.active || state.label == "online" {
                    "● "
                } else {
                    "○ "
                };
                let name_style = if state.active { bold() } else { dim() };
                lines.push(Line::from(vec![
                    Span::styled(branch, dim()),
                    Span::styled(dot, Style::default().fg(state.color)),
                    Span::styled(format!("@{nick}"), name_style),
                    Span::styled(format!("  ({spec}{count_suffix})  "), dim()),
                    Span::styled(state.label, Style::default().fg(state.color)),
                ]));
                let worker_tasks = group_member_tasks(snap, group, nick);
                for (j, task) in worker_tasks.iter().enumerate() {
                    let sub_branch = if j + 1 < worker_tasks.len() {
                        "├─ "
                    } else {
                        "└─ "
                    };
                    let kind_label = if task.parent.is_some() {
                        format!("sub/{}", task.kind)
                    } else {
                        task.kind.clone()
                    };
                    lines.push(Line::from(vec![
                        Span::styled(format!("{cont}  {sub_branch}"), dim()),
                        Span::styled(
                            format!("{:<12} ", task.status),
                            Style::default().fg(status_color(&task.status)),
                        ),
                        priority_span(&task.priority),
                        Span::raw(format!("[{kind_label}] {}", task.title)),
                        Span::styled(format!("  {}", short_id(&task.id)), dim()),
                    ]));
                }
            }
            None => {
                let pool_tag = if worker.count == 1 {
                    "[pool]".to_string()
                } else {
                    format!("[pool ×{}]", worker.count)
                };
                lines.push(Line::from(vec![
                    Span::styled(branch, dim()),
                    Span::styled("○ ", dim()),
                    Span::styled(format!("{pool_tag} "), Style::default().fg(Color::Cyan)),
                    Span::styled(spec, dim()),
                ]));
            }
        }
    }

    if !other_subtasks.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("└─ ", dim()),
            Span::styled("○ ", Style::default().fg(Color::Yellow)),
            Span::styled(
                format!("(unassigned / other)  {} subtask(s)", other_subtasks.len()),
                Style::default().fg(Color::Yellow),
            ),
        ]));
    }

    lines
}

fn draw_groups(frame: &mut Frame, app: &mut App, snap: &Snapshot, area: Rect) {
    app.group_rows.clear();
    app.group_subtask_rows.clear();
    app.group_idx = app.group_idx.min(snap.groups.len().saturating_sub(1));

    let groups_width = if area.width < 70 {
        Constraint::Percentage(35)
    } else {
        Constraint::Length(GROUPS_PANE_WIDTH)
    };
    let [groups_area, right] = Layout::horizontal([groups_width, Constraint::Min(10)]).areas(area);

    let groups_border = if app.groups_focus == GroupsFocus::Groups {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    };
    let groups_block = Block::bordered()
        .border_style(groups_border)
        .title(format!(" Groups ({}) ", snap.groups.len()));

    if snap.groups.is_empty() {
        let empty = vec![
            Line::from(Span::styled("No groups configured yet.", dim())),
            Line::from(""),
            Line::from(vec![
                Span::raw("Press "),
                Span::styled("n", bold().fg(Color::Cyan)),
                Span::raw(" to create a group,"),
            ]),
            Line::from(Span::styled("or use the CLI:", dim())),
            Line::from(Span::styled("  airc group create <name>", dim())),
            Line::from(Span::styled("    --lead <tpl>:<model>", dim())),
            Line::from(Span::styled("    --worker <tpl>:<model>", dim())),
        ];
        frame.render_widget(Paragraph::new(empty).block(groups_block), groups_area);
        let [topo_area, subtasks_area] =
            Layout::vertical([Constraint::Percentage(55), Constraint::Min(0)]).areas(right);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Select a group to view its topology.",
                dim(),
            )))
            .block(Block::bordered().title(" Topology ")),
            topo_area,
        );
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled("No active subtasks.", dim())))
                .block(Block::bordered().title(" Current Subtasks (0) ")),
            subtasks_area,
        );
        return;
    }

    let visible_groups = (groups_area.height.saturating_sub(2) as usize / 2).max(1);
    if app.group_idx < app.group_scroll {
        app.group_scroll = app.group_idx;
    } else if app.group_idx >= app.group_scroll + visible_groups {
        app.group_scroll = app.group_idx + 1 - visible_groups;
    }

    let mut group_lines: Vec<Line> = Vec::new();
    for (index, group) in snap
        .groups
        .iter()
        .enumerate()
        .skip(app.group_scroll)
        .take(visible_groups)
    {
        let (active_workers, total_workers) = group_active_worker_count(snap, group);
        let subtask_count = group_subtasks(snap, group).len();
        let lead_label = group
            .lead
            .nick
            .as_deref()
            .map(|n| format!("@{n}"))
            .unwrap_or_else(|| "(unnamed)".to_string());
        let projects_label = if group.projects.is_empty() {
            "(unattached)".to_string()
        } else {
            group.projects.join(", ")
        };
        let marker = if index == app.group_idx { "> " } else { "  " };
        let mut line1 = Line::from(vec![
            Span::raw(marker),
            Span::styled(group.name.clone(), bold().fg(Color::Cyan)),
            Span::raw("  "),
            Span::styled(
                projects_label,
                if group.projects.is_empty() {
                    dim()
                } else {
                    Style::default().fg(Color::Yellow)
                },
            ),
        ]);
        let active_style = if active_workers > 0 {
            Style::default().fg(Color::Green)
        } else {
            dim()
        };
        let mut line2 = Line::from(vec![
            Span::styled(format!("  {lead_label} · "), dim()),
            Span::styled(format!("{active_workers}/{total_workers} active"), active_style),
            Span::styled(format!(" · {subtask_count} sub"), dim()),
        ]);
        if index == app.group_idx {
            line1 = line1.style(Style::default().bg(Color::DarkGray));
            line2 = line2.style(Style::default().bg(Color::DarkGray));
        }
        let base_y = groups_area.y + 1 + group_lines.len() as u16;
        app.group_rows.push((base_y, index));
        app.group_rows.push((base_y + 1, index));
        group_lines.push(line1);
        group_lines.push(line2);
    }
    frame.render_widget(Paragraph::new(group_lines).block(groups_block), groups_area);

    let group = &snap.groups[app.group_idx];
    let subtasks = group_subtasks(snap, group);
    let topo_lines = group_topology_lines(snap, group, &subtasks);
    let max_topo_h = (right.height * 3 / 5).max(6).min(right.height);
    let topo_h = (topo_lines.len() as u16 + 2).clamp(6.min(right.height), max_topo_h);
    let [topo_area, subtasks_area] =
        Layout::vertical([Constraint::Length(topo_h), Constraint::Min(0)]).areas(right);

    frame.render_widget(
        Paragraph::new(topo_lines)
            .block(Block::bordered().title(format!(" Topology: {} ", group.name))),
        topo_area,
    );

    let subtasks_border = if app.groups_focus == GroupsFocus::Subtasks {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    };
    app.group_subtask_cursor = app
        .group_subtask_cursor
        .min(subtasks.len().saturating_sub(1));
    let visible_subtasks = (subtasks_area.height.saturating_sub(2) as usize / 2).max(1);
    if app.group_subtask_cursor < app.group_subtask_scroll {
        app.group_subtask_scroll = app.group_subtask_cursor;
    } else if app.group_subtask_cursor >= app.group_subtask_scroll + visible_subtasks {
        app.group_subtask_scroll = app.group_subtask_cursor + 1 - visible_subtasks;
    }

    let mut subtask_lines: Vec<Line> = Vec::new();
    for (index, sub) in subtasks
        .iter()
        .enumerate()
        .skip(app.group_subtask_scroll)
        .take(visible_subtasks)
    {
        let base_y = subtasks_area.y + 1 + subtask_lines.len() as u16;
        app.group_subtask_rows.push((base_y, index));
        app.group_subtask_rows.push((base_y + 1, index));
        let parent_text = match &sub.parent_title {
            Some(title) => format!("parent: {} \"{title}\"", short_id(&sub.parent_id)),
            None => format!("parent: {}", short_id(&sub.parent_id)),
        };
        let review_text = sub
            .reviewer
            .as_deref()
            .map(|r| format!(" · review @{r}"))
            .unwrap_or_default();
        let mut line1 = Line::from(vec![
            Span::raw("  "),
            Span::styled(
                format!("{:<15}", sub.status),
                Style::default().fg(status_color(&sub.status)),
            ),
            Span::styled(format!("@{:<12} ", sub.assignee), bold()),
            priority_span(&sub.priority),
            Span::raw(format!("[{}] {}", sub.kind, sub.title)),
        ]);
        let mut line2 = Line::from(vec![
            Span::styled(
                format!("    {} · {parent_text}{review_text}", short_id(&sub.task_id)),
                dim(),
            ),
        ]);
        if app.groups_focus == GroupsFocus::Subtasks && index == app.group_subtask_cursor {
            line1 = line1.style(Style::default().bg(Color::DarkGray));
            line2 = line2.style(Style::default().bg(Color::DarkGray));
        }
        subtask_lines.push(line1);
        subtask_lines.push(line2);
    }
    if subtasks.is_empty() {
        subtask_lines.push(Line::from(Span::styled(
            "No active subtasks for this group.",
            dim(),
        )));
    }
    frame.render_widget(
        Paragraph::new(subtask_lines).block(
            Block::bordered().border_style(subtasks_border).title(format!(
                " Current Subtasks ({})  (Tab pane · Enter details) ",
                subtasks.len()
            )),
        ),
        subtasks_area,
    );
}

fn chat_message<'a>(message: &'a ChatLine, me: &str, highlighted: bool) -> Text<'a> {
    let name_color = if message.sender == "aircd" {
        Color::DarkGray
    } else if message.sender.eq_ignore_ascii_case(me) {
        Color::Green
    } else {
        Color::Cyan
    };
    let mut body = Text::from(message.body.as_str()).lines.into_iter();
    let mut spans = Vec::new();
    if highlighted {
        spans.push(Span::styled("» ", bold().fg(Color::Yellow)));
        spans.push(Span::styled(clock(message.time), bold().fg(Color::Yellow)));
    } else {
        spans.push(Span::styled(clock(message.time), dim()));
    }
    spans.push(Span::styled(
        format!(" {}: ", message.sender),
        bold().fg(name_color),
    ));
    let mut first = Line::from(spans);
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
    let mut messages: Vec<(Paragraph, usize)> = Vec::new();
    let mut jump_target_row: Option<usize> = None;
    let mut running_rows = 0usize;
    let jump_seq = app
        .jump_to_seq
        .as_ref()
        .filter(|(room, _)| room == &current)
        .map(|(_, seq)| *seq);
    for message in snap.messages.get(&current).into_iter().flatten() {
        let highlighted = app
            .chat_highlight
            .as_ref()
            .is_some_and(|(room, seq)| room == &current && *seq == message.seq);
        if jump_seq == Some(message.seq) {
            jump_target_row = Some(running_rows);
        }
        for line in chat_message(message, &snap.me, highlighted).lines {
            let paragraph = Paragraph::new(line).wrap(Wrap { trim: false });
            let rows = paragraph.line_count(inner.width);
            running_rows += rows;
            messages.push((paragraph, rows));
        }
    }
    let total_rows = running_rows;
    app.chat_max_scroll = total_rows.saturating_sub(usize::from(inner.height));
    if let Some(start_row) = jump_target_row {
        let desired_skipped = start_row.saturating_sub(usize::from(inner.height) / 3);
        app.chat_scroll = app.chat_max_scroll.saturating_sub(desired_skipped);
        app.jump_to_seq = None;
    } else if app.chat_scroll > 0 && inner.width == app.chat_area.width {
        // Keep the same history in view when refresh appends messages.
        app.chat_scroll = app
            .chat_scroll
            .saturating_add(total_rows.saturating_sub(app.chat_content_height));
    }
    app.chat_content_height = total_rows;
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
    let width = width.min(area.width.saturating_sub(2).max(1)).min(area.width);
    let height = height.min(area.height.saturating_sub(1).max(1)).min(area.height);
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}

fn draw_picker_box(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    notice: Option<(&str, Style)>,
    items: &[(String, String)],
    idx: usize,
    footer: Option<&str>,
) {
    let frame_style = Style::default().fg(Color::Cyan);
    let mut lines: Vec<Line> = Vec::new();
    let mut skip_from = if items.is_empty() {
        0
    } else {
        idx.min(items.len() - 1)
    };
    if let Some((text, style)) = notice {
        lines.push(Line::from(Span::styled(format!("  {text}"), style)));
        if !items.is_empty() {
            skip_from += 1;
        }
    }
    for (i, (label, _)) in items.iter().enumerate() {
        if i == idx {
            lines.push(Line::from(Span::styled(
                format!("> {label}"),
                bold().fg(Color::Black).bg(Color::Cyan),
            )));
        } else {
            lines.push(Line::from(format!("  {label}")));
        }
    }
    let footer_rows = usize::from(footer.is_some());
    let widest = lines
        .iter()
        .map(|l| l.width())
        .chain(footer.iter().map(|f| f.chars().count() + 2))
        .max()
        .unwrap_or(10)
        .max(title.chars().count() + 4);
    let rect = centered(
        area,
        widest as u16 + 6,
        (lines.len() + footer_rows) as u16 + 2,
    );
    let visible = rect.height.saturating_sub(2 + footer_rows as u16) as usize;
    let first = skip_from.saturating_sub(visible.saturating_sub(1));
    let mut shown: Vec<Line> = lines.into_iter().skip(first).take(visible).collect();
    if let Some(hint) = footer {
        shown.push(Line::from(Span::styled(format!(" {hint}"), dim())));
    }
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(shown).block(
            Block::bordered()
                .border_style(frame_style)
                .title(format!(" {title} ")),
        ),
        rect,
    );
}

fn draw_prompt_box(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    text: &str,
    hint: Option<&str>,
    error: Option<&str>,
) {
    let frame_style = Style::default().fg(Color::Cyan);
    let extra = error.or(hint);
    let height = if extra.is_some() { 4 } else { 3 };
    let rect = centered(area, 78, height);
    let mut lines = vec![Line::from(text.to_string())];
    if let Some(err) = error {
        lines.push(Line::from(Span::styled(
            format!("! {err}"),
            Style::default().fg(Color::Red),
        )));
    } else if let Some(h) = hint {
        lines.push(Line::from(Span::styled(h.to_string(), dim())));
    }
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .border_style(frame_style)
                .title(format!(" {title} ")),
        ),
        rect,
    );
    if rect.width >= 3 && rect.height >= 3 {
        let cursor_x =
            rect.x + 1 + (text.chars().count() as u16).min(rect.width.saturating_sub(3));
        frame.set_cursor_position((cursor_x, rect.y + 1));
    }
}

fn format_editor_seat(seat: &GroupEditorSeat, is_lead: bool) -> String {
    if seat.template.is_empty() && seat.model.is_empty() {
        return "(not set — Enter to configure)".to_string();
    }
    let spec = match (seat.template.is_empty(), seat.model.is_empty()) {
        (true, true) => "-".to_string(),
        (false, true) => seat.template.clone(),
        (true, false) => seat.model.clone(),
        (false, false) => format!("{}:{}", seat.template, seat.model),
    };
    let nick = seat
        .nick
        .as_deref()
        .map(clean_nick)
        .filter(|s| !s.is_empty())
        .map(|n| format!("@{n}"))
        .unwrap_or_else(|| {
            if is_lead {
                "(unnamed)".to_string()
            } else {
                "[pool]".to_string()
            }
        });
    if is_lead {
        format!("★ {spec}  {nick}")
    } else {
        format!("{spec}  ×{}  {nick}", seat.count)
    }
}

fn draw_seat_editor(
    frame: &mut Frame,
    area: Rect,
    seat_ed: &GroupSeatEditor,
    snap: &Snapshot,
) {
    let target_label = match seat_ed.target {
        None => "Lead seat".to_string(),
        Some(i) => format!("Worker seat {}", i + 1),
    };
    match seat_ed.step {
        SeatStep::Menu { row } => {
            let mut items = vec![
                (format!("Template : {}", seat_ed.seat.template), "template".to_string()),
                (format!("Model    : {}", seat_ed.seat.model), "model".to_string()),
            ];
            if seat_ed.target.is_some() {
                items.push((format!("Count    : {}", seat_ed.seat.count), "count".to_string()));
            }
            let nick_label = seat_ed
                .seat
                .nick
                .as_deref()
                .map(clean_nick)
                .filter(|s| !s.is_empty())
                .map(|n| format!("@{n}"))
                .unwrap_or_else(|| "(pool / unnamed)".to_string());
            items.push((format!("Nick     : {nick_label}"), "nick".to_string()));
            items.push(("[ Done ]".to_string(), "done".to_string()));
            let hint = if seat_ed.target.is_some() {
                "Enter edit · +/- count · Esc cancel"
            } else {
                "Enter edit · Esc cancel"
            };
            draw_picker_box(
                frame,
                area,
                &target_label,
                seat_ed
                    .error
                    .as_deref()
                    .map(|e| (e, Style::default().fg(Color::Red))),
                &items,
                row,
                Some(hint),
            );
        }
        SeatStep::PickTemplate { idx } => {
            let items = template_picker_items(snap, &seat_ed.seat.template);
            draw_picker_box(
                frame,
                area,
                &format!("{target_label}: client template"),
                None,
                &items,
                idx,
                Some("Enter select · Esc back"),
            );
        }
        SeatStep::TypeTemplate => {
            draw_prompt_box(
                frame,
                area,
                &format!("{target_label}: client template id"),
                &seat_ed.text,
                Some("Letters, digits, '-' or '_' (e.g. claude, codex, gemini, ollama)"),
                seat_ed.error.as_deref(),
            );
        }
        SeatStep::PickModel { idx } => {
            let (items, problem) =
                seat_model_items(snap, &seat_ed.seat.template, &seat_ed.seat.model);
            draw_picker_box(
                frame,
                area,
                &format!("{target_label}: model for {}", seat_ed.seat.template),
                problem
                    .as_deref()
                    .map(|p| (p, Style::default().fg(Color::Yellow))),
                &items,
                idx,
                Some("Enter select · Esc back"),
            );
        }
        SeatStep::TypeModel => {
            draw_prompt_box(
                frame,
                area,
                &format!("{target_label}: model name"),
                &seat_ed.text,
                Some("Model id expected by the template (e.g. opus, sonnet, qwen3:35b)"),
                seat_ed.error.as_deref(),
            );
        }
        SeatStep::TypeCount => {
            draw_prompt_box(
                frame,
                area,
                &format!("{target_label}: worker seat count"),
                &seat_ed.text,
                Some("Positive integer (>= 1)"),
                seat_ed.error.as_deref(),
            );
        }
        SeatStep::PickNick { idx } => {
            let items = nick_picker_items(snap, seat_ed.seat.nick.as_deref());
            draw_picker_box(
                frame,
                area,
                &format!("{target_label}: account nick (optional)"),
                None,
                &items,
                idx,
                Some("Enter select · Esc back"),
            );
        }
        SeatStep::TypeNick => {
            draw_prompt_box(
                frame,
                area,
                &format!("{target_label}: account nick (blank for pool)"),
                &seat_ed.text,
                Some("Optional account name without spaces (or blank for pool seat)"),
                seat_ed.error.as_deref(),
            );
        }
    }
}

fn draw_group_editor(
    frame: &mut Frame,
    area: Rect,
    editor: &GroupEditorState,
    snap: &Snapshot,
) {
    let title = if editor.is_edit {
        format!(" Edit group: {} ", editor.name)
    } else {
        " New group ".to_string()
    };
    let fields = editor.fields();
    let cursor = editor.cursor.min(fields.len().saturating_sub(1));
    let or_hint = |val: &str, empty_label: &str| {
        if val.trim().is_empty() {
            empty_label.to_string()
        } else {
            val.to_string()
        }
    };

    let rows: Vec<Line> = fields
        .iter()
        .enumerate()
        .map(|(i, field)| {
            let selected = i == cursor;
            let text = match field {
                GroupEditorField::Name => {
                    if editor.is_edit {
                        format!("Name          : {}  (locked)", editor.name)
                    } else {
                        format!("Name          : {}", or_hint(&editor.name, "(required)"))
                    }
                }
                GroupEditorField::Description => {
                    format!(
                        "Description   : {}",
                        or_hint(&editor.description, "(optional)")
                    )
                }
                GroupEditorField::Lead => {
                    format!("Lead          : {}", format_editor_seat(&editor.lead, true))
                }
                GroupEditorField::Worker(idx) => {
                    format!(
                        "Worker {:<7}: {}",
                        idx + 1,
                        format_editor_seat(&editor.workers[*idx], false)
                    )
                }
                GroupEditorField::AddWorker => "+ Add worker seat".to_string(),
                GroupEditorField::TimeoutSecs => {
                    format!("Timeout (s)   : {}", or_hint(&editor.timeout_secs, "(off)"))
                }
                GroupEditorField::ReviewPeers => {
                    format!("Review peers  : {}", or_hint(&editor.review_peers, "(none)"))
                }
                GroupEditorField::ReviewQuorum => {
                    format!(
                        "Review quorum : {}",
                        or_hint(&editor.review_quorum, "(all peers)")
                    )
                }
                GroupEditorField::LeadPrompt => {
                    format!("Lead prompt   : {}", or_hint(&editor.lead_prompt, "(none)"))
                }
                GroupEditorField::WorkerPrompt => {
                    format!(
                        "Worker prompt : {}",
                        or_hint(&editor.worker_prompt, "(none)")
                    )
                }
                GroupEditorField::Save => "[ Save Group ]".to_string(),
                GroupEditorField::Cancel => "[ Cancel ]".to_string(),
            };
            if selected {
                Line::from(Span::styled(
                    format!("> {text}"),
                    bold().fg(Color::Black).bg(Color::Cyan),
                ))
            } else if matches!(field, GroupEditorField::Save) {
                Line::from(Span::styled(format!("  {text}"), bold().fg(Color::Green)))
            } else if matches!(field, GroupEditorField::AddWorker) {
                Line::from(Span::styled(format!("  {text}"), Style::default().fg(Color::Cyan)))
            } else {
                Line::from(format!("  {text}"))
            }
        })
        .collect();

    let rect = centered(area, 84, rows.len() as u16 + 4);
    let visible = rect.height.saturating_sub(3) as usize;
    let first = cursor.saturating_sub(visible.saturating_sub(1));
    let mut shown: Vec<Line> = rows.into_iter().skip(first).take(visible).collect();
    if let Some(err) = &editor.error {
        shown.push(Line::from(Span::styled(
            format!(" ! {err}"),
            Style::default().fg(Color::Red),
        )));
    } else {
        shown.push(Line::from(Span::styled(
            " Up/Down move · Enter edit · +/- count · d remove worker · s save · Esc cancel",
            dim(),
        )));
    }

    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(shown).block(
            Block::bordered()
                .border_style(Style::default().fg(Color::Cyan))
                .title(title),
        ),
        rect,
    );

    if let Some(sub) = &editor.sub {
        match sub {
            GroupEditorSub::Text { field, text } => {
                let (title, hint) = match field {
                    GroupEditorField::Name => ("Group name", "Unique name without '/' or '\\'"),
                    GroupEditorField::Description => ("Description (optional)", "Enter to keep, Esc to cancel"),
                    GroupEditorField::TimeoutSecs => (
                        "Inactivity timeout (seconds, optional)",
                        "Positive integer or blank to disable",
                    ),
                    GroupEditorField::ReviewPeers => (
                        "Peer reviewers (optional)",
                        "Comma-separated named seats (e.g. lead, worker1) or blank",
                    ),
                    GroupEditorField::ReviewQuorum => (
                        "Review quorum (optional)",
                        "Integer 1..N, or blank for all peers",
                    ),
                    GroupEditorField::LeadPrompt => ("Lead prompt (optional)", "Custom prompt override or blank"),
                    GroupEditorField::WorkerPrompt => ("Worker prompt (optional)", "Custom prompt override or blank"),
                    _ => ("Edit", ""),
                };
                draw_prompt_box(frame, area, title, text, Some(hint), None);
            }
            GroupEditorSub::Seat(seat_ed) => draw_seat_editor(frame, area, seat_ed, snap),
        }
    }
}

fn draw_group_attach(
    frame: &mut Frame,
    area: Rect,
    group_name: &str,
    attached: &[String],
    selected: &[String],
    idx: usize,
    snap: &Snapshot,
) {
    let title = format!("Attach group '{group_name}' to project");
    if snap.projects.is_empty() {
        draw_picker_box(
            frame,
            area,
            &title,
            Some(("No projects exist on the hub yet.", dim())),
            &[],
            0,
            Some("Esc close"),
        );
        return;
    }
    let items: Vec<(String, String)> = snap
        .projects
        .iter()
        .map(|p| {
            let badge = if attached.iter().any(|c| c == &p.channel) {
                "[attached]"
            } else if selected.contains(&p.channel) {
                "[selected]"
            } else {
                "[        ]"
            };
            let pm = p
                .pm
                .as_deref()
                .map(|m| format!("  (PM @{m})"))
                .unwrap_or_default();
            (
                format!("{badge} {:<16} {}{pm}", p.channel, p.name),
                p.channel.clone(),
            )
        })
        .collect();
    draw_picker_box(
        frame,
        area,
        &title,
        None,
        &items,
        idx.min(items.len().saturating_sub(1)),
        Some("Up/Down move · Space multi-select · Enter attach · Esc close"),
    );
}

fn highlight_spans(
    text: &str,
    terms: &[String],
    base_style: Style,
    hit_style: Style,
) -> Vec<Span<'static>> {
    if terms.is_empty() || text.is_empty() {
        return vec![Span::styled(text.to_string(), base_style)];
    }
    let lower = text.to_ascii_lowercase();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for term in terms {
        if term.is_empty() {
            continue;
        }
        let mut offset = 0;
        while let Some(pos) = lower[offset..].find(term.as_str()) {
            let start = offset + pos;
            let end = start + term.len();
            ranges.push((start, end));
            offset = end;
        }
    }
    if ranges.is_empty() {
        return vec![Span::styled(text.to_string(), base_style)];
    }
    ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        if let Some(last) = merged.last_mut() {
            if start <= last.1 {
                last.1 = last.1.max(end);
                continue;
            }
        }
        merged.push((start, end));
    }
    let mut spans = Vec::new();
    let mut cursor = 0;
    for (start, end) in merged {
        if let Some(prefix) = text.get(cursor..start).filter(|s| !s.is_empty()) {
            spans.push(Span::styled(prefix.to_string(), base_style));
        }
        if let Some(matched) = text.get(start..end).filter(|s| !s.is_empty()) {
            spans.push(Span::styled(matched.to_string(), hit_style));
        }
        cursor = end;
    }
    if let Some(tail) = text.get(cursor..).filter(|s| !s.is_empty()) {
        spans.push(Span::styled(tail.to_string(), base_style));
    }
    spans
}

fn draw_search_modal(frame: &mut Frame, area: Rect, state: &SearchState, snap: &Snapshot) {
    let rect = centered(area, 96, 26);
    frame.render_widget(Clear, rect);
    let block = Block::bordered()
        .border_style(Style::default().fg(Color::Cyan))
        .title(" Search Chat History  (Tab scope · Up/Down select · Enter jump · Esc close) ");
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let parsed = ParsedSearchQuery::parse(&state.query);
    let hits = search_hits(state, snap);
    let idx = state.idx.min(hits.len().saturating_sub(1));
    let selected_hit = hits.get(idx);

    let show_preview = inner.height >= 10 && selected_hit.is_some();
    let preview_height = if show_preview { 4 } else { 0 };
    let [query_area, scope_area, sep_area, results_area, preview_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(preview_height),
    ])
    .areas(inner);

    // Query line.
    let query_line = Line::from(vec![
        Span::styled("Query: ", bold().fg(Color::Cyan)),
        Span::raw(state.query.clone()),
    ]);
    frame.render_widget(Paragraph::new(query_line), query_area);
    if query_area.width > 7 {
        let cursor_x = query_area.x
            + 7
            + (state.query.chars().count() as u16).min(query_area.width.saturating_sub(8));
        frame.set_cursor_position((cursor_x, query_area.y));
    }

    // Scope bar + match count.
    let mut scopes = vec![
        (SearchScope::All, "All".to_string()),
        (SearchScope::Sprints, "Sprints".to_string()),
        (SearchScope::DirectMessages, "DMs".to_string()),
    ];
    if let Some(room) = &state.current_room {
        scopes.push((SearchScope::CurrentRoom, format!("Room: {room}")));
    }
    let mut scope_spans = vec![Span::styled("Scope: ", dim())];
    for (scope, label) in scopes {
        let active = scope == state.scope;
        scope_spans.push(Span::styled(
            format!(" {label} "),
            if active {
                bold().fg(Color::Black).bg(Color::Cyan)
            } else {
                dim()
            },
        ));
        scope_spans.push(Span::raw(" "));
    }
    let count_text = if parsed.is_empty() {
        String::new()
    } else if hits.is_empty() {
        "  0 matches".to_string()
    } else {
        format!("  {} match(es) [{}/{}]", hits.len(), idx + 1, hits.len())
    };
    scope_spans.push(Span::styled(count_text, Style::default().fg(Color::Yellow)));
    frame.render_widget(Paragraph::new(Line::from(scope_spans)), scope_area);

    frame.render_widget(
        Paragraph::new(Span::styled(
            "─".repeat(usize::from(sep_area.width)),
            dim(),
        )),
        sep_area,
    );

    // Results list or empty/prompt state.
    if parsed.is_empty() {
        let sprint_count = snap
            .messages
            .keys()
            .filter(|r| !r.starts_with('@') && project_for(snap, r).is_some_and(|p| p.channel != **r))
            .count();
        let dm_count = snap.messages.keys().filter(|r| r.starts_with('@')).count();
        let msg_total: usize = snap.messages.values().map(Vec::len).sum();
        let help_lines = vec![
            Line::from("Type keywords to search conversation history across sprint channels and DMs."),
            Line::from(""),
            Line::from(vec![
                Span::styled("  Syntax: ", bold().fg(Color::Cyan)),
                Span::raw("term1 term2 (AND)   from:<nick>   in:<room>"),
            ]),
            Line::from(vec![
                Span::styled("  Keys:   ", bold().fg(Color::Cyan)),
                Span::raw("Tab cycle scope · Up/Down pick · Ctrl+U clear · Enter jump · Esc close"),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                format!(
                    "  Indexed locally: {msg_total} messages ({sprint_count} sprint channel(s), {dm_count} DM thread(s))"
                ),
                dim(),
            )),
        ];
        frame.render_widget(Paragraph::new(help_lines).wrap(Wrap { trim: false }), results_area);
    } else if hits.is_empty() {
        let mut msg_lines = vec![Line::from(Span::styled(
            format!("No messages matched \"{}\".", state.query.trim()),
            dim(),
        ))];
        if let Some(err) = snap
            .search_results
            .as_ref()
            .and_then(|r| r.error.as_deref())
        {
            msg_lines.push(Line::from(Span::styled(
                format!("Server search note: {err}"),
                Style::default().fg(Color::Yellow),
            )));
        }
        frame.render_widget(Paragraph::new(msg_lines).wrap(Wrap { trim: false }), results_area);
    } else {
        let visible = usize::from(results_area.height);
        let first = idx.saturating_sub(visible.saturating_sub(1));
        let mut rows: Vec<Line> = Vec::new();
        for (i, hit) in hits.iter().enumerate().skip(first).take(visible) {
            let selected = i == idx;
            let row_bg = if selected {
                Style::default().bg(Color::DarkGray)
            } else {
                Style::default()
            };
            let room_color = if hit.is_dm {
                Color::Magenta
            } else if hit.is_sprint {
                Color::Cyan
            } else {
                Color::Blue
            };
            let collapsed = hit.body.replace('\n', " ");
            let mut spans = vec![
                Span::styled(if selected { "> " } else { "  " }, row_bg.add_modifier(Modifier::BOLD)),
                Span::styled(format!("{} ", clock(hit.time)), dim().patch(row_bg)),
                Span::styled(
                    format!("{:<16} ", hit.room),
                    row_bg.fg(room_color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("@{:<12} ", hit.sender),
                    row_bg.fg(Color::Green),
                ),
            ];
            spans.extend(highlight_spans(
                &collapsed,
                &parsed.terms,
                row_bg,
                row_bg.fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ));
            rows.push(Line::from(spans).style(row_bg));
        }
        frame.render_widget(Paragraph::new(rows), results_area);
    }

    // Bottom preview pane for the currently selected search hit.
    if show_preview {
        if let Some(hit) = selected_hit {
            let [p_sep, p_body] =
                Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(preview_area);
            let header = format!(
                "── Preview: {} · @{} · {} (seq #{}) ",
                hit.room,
                hit.sender,
                clock(hit.time),
                hit.seq
            );
            let fill = usize::from(p_sep.width).saturating_sub(header.chars().count());
            frame.render_widget(
                Paragraph::new(Span::styled(
                    format!("{header}{}", "─".repeat(fill)),
                    dim(),
                )),
                p_sep,
            );
            let preview_lines: Vec<Line> = hit
                .body
                .lines()
                .map(|line| {
                    Line::from(highlight_spans(
                        line,
                        &parsed.terms,
                        Style::default(),
                        bold().fg(Color::Yellow),
                    ))
                })
                .collect();
            frame.render_widget(
                Paragraph::new(preview_lines).wrap(Wrap { trim: false }),
                p_body,
            );
        }
    }
}

fn draw_modal(frame: &mut Frame, modal: &Modal, snap: &Snapshot, area: Rect) {
    let frame_style = Style::default().fg(Color::Cyan);
    match modal {
        Modal::Picker {
            title, items, idx, ..
        } => {
            draw_picker_box(frame, area, title, None, items, *idx, None);
        }
        Modal::Prompt { title, text, .. } => {
            draw_prompt_box(frame, area, title, text, None, None);
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
        Modal::GroupEditor(editor) => draw_group_editor(frame, area, editor, snap),
        Modal::GroupAttach {
            group_name,
            attached,
            selected,
            idx,
        } => draw_group_attach(frame, area, group_name, attached, selected, *idx, snap),
        Modal::Search(state) => draw_search_modal(frame, area, state, snap),
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
    let mut tls = TuiTlsOverrides::default();
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--server" => server = Some(iter.next().context("--server requires a URL")?),
            "--profile" => profile = Some(iter.next().context("--profile requires a name")?),
            "--tls-ca" => tls.ca = Some(iter.next().context("--tls-ca requires a path")?),
            "--tls-cert" => tls.cert = Some(iter.next().context("--tls-cert requires a path")?),
            "--tls-key" => tls.key = Some(iter.next().context("--tls-key requires a path")?),
            "--tls-insecure" => tls.insecure = true,
            "--dump" => dump = true,
            "-h" | "--help" => {
                println!(
                    "aircd tui - dashboard, chat and control panel for a running hub\n\
                     Flags:\n  \
                       --server <url>     Hub address (default: the profile's, or http://127.0.0.1:6680)\n  \
                       --profile <name>   Saved airc profile to sign in as (default: your default profile)\n  \
                       --tls-ca <path>    CA certificate PEM for verifying HTTPS hub\n  \
                       --tls-cert <path>  Client TLS certificate PEM for mutual TLS (mTLS)\n  \
                       --tls-key <path>   Client TLS private key PEM for mutual TLS (mTLS)\n  \
                       --tls-insecure     Skip HTTPS server certificate verification\n  \
                       --dump             Print the dashboard and chat screens once as plain text and exit\n\
                     Press ? on the dashboard for the list of keys."
                );
                return Ok(());
            }
            other => return Err(anyhow!("unknown argument for tui: {other} (use --help)")),
        }
    }
    let cfg = load_config(server, profile, tls)?;
    let client = build_http_client(&cfg)?;

    let shared = Arc::new(Mutex::new(Snapshot {
        server: cfg.server.clone(),
        me: cfg.nick.clone(),
        ..Snapshot::default()
    }));
    // Fail with a clear message before taking over the terminal, except
    // when the hub is simply not running: the interface offers to start it.
    if let Err(err) = refresh(&client, &cfg, &shared).await {
        if dump || !hub_unreachable(&err) {
            return Err(err);
        }
        let mut snap = shared.lock().expect("snapshot mutex poisoned");
        snap.hub_down = true;
        snap.error = Some(format!("{err:#}"));
    }

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
                view: View::Groups,
                ..App::default()
            },
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
    let fetcher = tokio::spawn(fetch_loop(client, cfg, shared.clone(), rx, stop.clone()));
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
            "#sample-app".to_string(),
            vec![ChatLine {
                seq: 1,
                sender: "agent3.8".to_string(),
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
            me: "user".to_string(),
            my_role: "leader".to_string(),
            accounts: vec![
                account("user", "leader", true, false),
                Account {
                    launcher: Some(true),
                    command_found: true,
                    last_run: Some("ok".into()),
                    ..account("agent3.8", "builder", false, true)
                },
                Account {
                    launcher: Some(false),
                    command_found: true,
                    ..account("agent-lite", "builder", false, false)
                },
            ],
            projects: vec![ProjectView {
                channel: "#sample-app".into(),
                name: "Demo project".into(),
                pm: Some("agent3.8".into()),
                tasks: vec![TaskView {
                    id: "task_abc123".into(),
                    kind: "bug".into(),
                    status: "blocked".into(),
                    assignee: "codex".into(),
                    title: "Fix seek bar".into(),
                    sprint: Some("#sample-app-s1".into()),
                    ..TaskView::default()
                }],
                members: vec![
                    Member {
                        nick: "agent3.8".into(),
                        role: "coordinator".into(),
                    },
                    Member {
                        nick: "codex".into(),
                        role: "builder".into(),
                    },
                ],
                repos: vec![RepoView {
                    name: "demo-project".into(),
                    folder_found: false,
                    local_ok: false,
                    branch: None,
                    remote: Some("https://github.com/example-org/sample-workspace.git".into()),
                }],
                rooms: vec![RoomView {
                    channel: "#sample-app-s1".into(),
                    label: "s1".into(),
                    title: Some("Local playback".into()),
                }],
            }],
            groups: Vec::new(),
            waiting: vec![Waiting {
                task_id: "task_abc123".into(),
                channel: "#sample-app".into(),
                asked_by: "codex".into(),
                question: "May I buy a licence?".into(),
                escalated: true,
            }],
            launchers: vec![Launcher {
                nick: "agent-lite".into(),
                enabled: false,
                mode: "fresh".into(),
            }],
            runs: vec!["ok       @agent3.8  role in #sample-app".into()],
            channels: vec![
                "#general".into(),
                "#sample-app".into(),
                "#sample-app-s1".into(),
            ],
            messages,
            dm_seq: 0,
            detail: None,
            templates: Vec::new(),
            models: None,
            search_results: None,
            notice: None,
            error: None,
            hub_down: false,
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
            "you: @user (leader)",
            "agent3.8",
            "working",
            "no launcher",
            "launcher off",
            "NEEDS YOU",
            "May I buy a licence?",
            "#sample-app",
            "PM @agent3.8",
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
            "[-] Demo project",
            "#sample-app",
            "codex needs you",
            "agent3.8 working",
            "14:05",
            "agent3.8: ",
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
            .position(|line| line.contains(" Message #sample-app "))
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
            "#sample-app".into(),
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
            .get_mut("#sample-app")
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
            .get_mut("#sample-app")
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
            app.input = "hi @ag".into();
            app.chat_scroll = 5;
            press(&mut app, &snap, &[key]);
            assert_eq!(app.chat_scroll, 0, "input key {key:?}");
        }
        app.input = "hello".into();
        app.chat_scroll = 5;
        assert_eq!(
            press(&mut app, &snap, &[KeyCode::Enter]),
            Some(Action::Send {
                target: "#sample-app".into(),
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
        snap.messages.get_mut("#sample-app").unwrap()[0].body = format!(
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
        snap.messages.get_mut("#sample-app").unwrap()[0].body = (0..66_000)
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
        snap.messages.get_mut("#sample-app").unwrap().truncate(1);
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
        type_text(&mut app, &snap, "ping @agent3");
        press(&mut app, &snap, &[KeyCode::Tab]);
        assert_eq!(app.input, "ping @agent3.8 ");
        assert_eq!(
            press(&mut app, &snap, &[KeyCode::Enter]),
            Some(Action::Send {
                target: "#sample-app".into(),
                body: "ping @agent3.8".into()
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
            complete_mention("hi @ag", &accounts).as_deref(),
            Some("hi @agent")
        );
        assert_eq!(
            complete_mention("hi @us", &accounts).as_deref(),
            Some("hi @user ")
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
        assert!(render(&mut app, &snap).contains("New task in #sample-app: title"));
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
                    "channel": "#sample-app", "title": "Add subtitles", "kind": "bug",
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
                json!({"assignee": "agent3.8"}),
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
                json!({"channel": "#sample-app", "nick": "agent3.8"}),
                "Project manager set"
            )
        );
        // Members: every account, members first with their role, then the
        // others, then a new account.
        press(&mut app, &snap, &[KeyCode::Char('m')]);
        let screen = render(&mut app, &snap);
        assert!(screen.contains("Members of #sample-app"), "{screen}");
        assert!(screen.contains("@agent3.8  (coordinator, change role)"), "{screen}");
        assert!(screen.contains("@agent-lite  (add)"), "{screen}");
        assert!(screen.contains("+ new account (client and model)"), "{screen}");
        assert!(
            screen.find("@agent3.8  (coordinator").unwrap() < screen.find("@user  (add)").unwrap()
                && screen.find("@agent-lite  (add)").unwrap() < screen.find("+ new account").unwrap(),
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
                json!({"channel": "#sample-app", "nick": "agent-lite", "role": "builder"}),
                "@agent-lite is now builder in #sample-app"
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
                body: json!({"action": "enable", "nick": "agent-lite"}),
                owner: true,
                done: "Launcher for @agent-lite: enabled".into(),
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
            "#sample-app",
            "PM @agent3.8",
            "repo demo-project",
            "folder missing",
            "on hub",
            "remote example-org/sample-workspace",
            "working",
            "launcher off",
            "no launcher",
            "[n] new project",
        ] {
            assert!(screen.contains(expected), "home is missing {expected:?}");
        }

        assert_eq!(press(&mut app, &snap, &[KeyCode::Enter]), None);
        assert_eq!(app.view, View::Dashboard);
        assert_eq!(app.project.as_deref(), Some("#sample-app"));
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
        app.project = Some("#sample-app".into());
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
        let state = |snap: &Snapshot, nick: &str| member_state(snap, "#sample-app", nick);
        // codex's task is blocked on a question escalated to the owner.
        assert_eq!(state(&snap, "codex"), Some((Color::Red, "needs you")));
        snap.waiting[0].escalated = false;
        assert_eq!(state(&snap, "codex"), Some((Color::Yellow, "waiting on PM")));
        // Answered: the dot follows the task's status.
        let question = snap.waiting.remove(0);
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
        assert_eq!(state(&snap, "agent3.8"), Some((Color::Green, "working")));
        assert_eq!(state(&snap, "user"), None);

        // An open question wins over the status the task shows.
        snap.waiting.push(question);
        snap.projects[0].tasks[0].status = "in_progress".into();
        assert_eq!(
            state(&snap, "codex"),
            Some((Color::Yellow, "waiting on PM"))
        );
        snap.waiting[0].escalated = true;
        assert_eq!(state(&snap, "codex"), Some((Color::Red, "needs you")));
        // A running agent with work it has not started shows as working,
        // but its review still shows as review.
        snap.waiting.clear();
        snap.projects[0].tasks[0].status = "assigned".into();
        snap.accounts.push(Account {
            nick: "codex".into(),
            running: true,
            ..Account::default()
        });
        assert_eq!(state(&snap, "codex"), Some((Color::Green, "working")));
        snap.projects[0].tasks[0].status = "review".into();
        assert_eq!(state(&snap, "codex"), Some((Color::Blue, "in review")));
    }

    #[test]
    fn hints_and_tabs_are_coloured_by_what_the_key_does() {
        assert_eq!(hint_color("q", "quit"), Color::Red);
        assert_eq!(hint_color("t", "test"), Color::Magenta);
        assert_eq!(hint_color("Esc", "home"), Color::Blue);
        assert_eq!(hint_color("w", "answer"), Color::Yellow);
        assert_eq!(hint_color("n", "new project"), Color::Green);

        // The tab bar keeps the exact TABS text, so clicks still line up,
        // and highlights the open view.
        for view in [View::Home, View::Dashboard, View::Groups, View::Chat] {
            let text: String = tab_spans(view).iter().map(|s| s.content.as_ref()).collect();
            assert_eq!(text, TABS);
        }
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30)).unwrap();
        let mut app = App {
            view: View::Dashboard,
            ..App::default()
        };
        let snap = fixture();
        terminal.draw(|frame| draw(frame, &mut app, &snap)).unwrap();
        let buffer = terminal.backend().buffer();
        let bg_at = |x: usize, y: u16| buffer[(x as u16, y)].bg;
        assert_eq!(bg_at(TABS.find("[D]").unwrap() + 4, 0), Color::Yellow);
        assert_eq!(bg_at(TABS.find("[H]").unwrap(), 0), Color::Cyan);
        assert_eq!(bg_at(TABS.find("[Q]").unwrap(), 0), Color::Red);
        // Footer: the [q] quit hint sits on red, [n] new on green.
        let footer: String = (0..120)
            .map(|x| buffer[(x, 29)].symbol().to_string())
            .collect();
        assert_eq!(bg_at(footer.find("[q]").unwrap(), 29), Color::Red);
        assert_eq!(bg_at(footer.find("[n]").unwrap(), 29), Color::Green);
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
        snap.messages.get_mut("#sample-app").unwrap().push(ChatLine {
            seq: 2,
            sender: "codex".into(),
            body: "done".into(),
            time: 0,
        });
        let screen = render(&mut app, &snap);
        let row = screen
            .lines()
            .find(|line| line.contains("#sample-app") && !line.contains("Message"))
            .unwrap()
            .to_string();
        assert!(row.contains(" 1"), "unread count missing in {row:?}");
        // Opening the room marks it read.
        press(&mut app, &snap, &[KeyCode::Down]);
        render(&mut app, &snap);
        assert_eq!(app.seen["#sample-app"], 2);
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
            dm_peer("@dm:agent3.8:user", "user").as_deref(),
            Some("agent3.8")
        );
        assert_eq!(dm_peer("@dm:codex:user", "UsEr").as_deref(), Some("codex"));
        assert_eq!(dm_peer("@dm:user:user", "user").as_deref(), Some("user"));
        assert_eq!(dm_peer("#general", "user"), None);
    }

    #[test]
    fn direct_messages_list_after_the_rooms_and_reply_privately() {
        let mut snap = fixture();
        snap.channels.push("@agent3.8".into());
        snap.messages.insert(
            "@agent3.8".into(),
            vec![dm(5, "agent3.8", "can you look at #19?")],
        );
        let mut app = App {
            view: View::Chat,
            channel_idx: 3,
            ..App::default()
        };
        let screen = render(&mut app, &snap);
        assert!(screen.contains("Direct messages"), "{screen}");
        assert!(screen.contains("can you look at #19?"), "{screen}");
        assert!(screen.contains("Message @agent3.8"), "{screen}");
        type_text(&mut app, &snap, "on it");
        assert_eq!(
            press(&mut app, &snap, &[KeyCode::Enter]),
            Some(Action::Send {
                target: "@agent3.8".into(),
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
        assert_eq!(nicks, ["agent3.8", "agent-lite"], "you cannot DM yourself");
        press(&mut app, &snap, &[KeyCode::Down, KeyCode::Enter]);
        assert!(app.modal.is_none());
        let screen = render(&mut app, &snap);
        assert!(screen.contains("Message @agent-lite"), "{screen}");
        assert_eq!(chat_rooms(&app, &snap)[app.channel_idx], "@agent-lite");
        type_text(&mut app, &snap, "hello");
        assert_eq!(
            press(&mut app, &snap, &[KeyCode::Enter]),
            Some(Action::Send {
                target: "@agent-lite".into(),
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
        assert!(app.people_rows.iter().all(|(_, nick)| nick != "user"));
        let (row, nick) = app
            .people_rows
            .iter()
            .find(|(_, nick)| nick == "agent-lite")
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
            .push(dm(10, "user", "sure"));
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
        for expected in ["[-] Demo project", "#sample-app", "s1 Local play", "Message #sample-app-s1"] {
            assert!(screen.contains(expected), "chat is missing {expected:?}");
        }
        // The project line names the current sprint and the task row its sprint.
        let dashboard = render(&mut App::default(), &snap);
        assert!(dashboard.contains("sprint s1"), "project line has no sprint");
        assert!(dashboard.contains("s1  [bug] Fix seek bar"), "task row has no sprint");

        // In a sprint room the dots come from that sprint's tasks only.
        assert_eq!(
            member_state(&snap, "#sample-app-s1", "codex"),
            Some((Color::Red, "needs you"))
        );
        snap.projects[0].tasks[0].sprint = Some("#sample-app-s2".into());
        assert_eq!(member_state(&snap, "#sample-app-s1", "codex"), None);
        assert!(member_state(&snap, "#sample-app", "codex").is_some());
    }

    #[test]
    fn o_opens_the_next_sprint_of_the_selected_project() {
        let snap = fixture();
        for mut app in [home(), App::default()] {
            press(&mut app, &snap, &[KeyCode::Char('o')]);
            assert!(render(&mut app, &snap).contains("Open the next sprint of #sample-app"));
            type_text(&mut app, &snap, "Subtitles");
            assert_eq!(
                press(&mut app, &snap, &[KeyCode::Enter]),
                sent(
                    "/v1/rooms",
                    json!({"project": "#sample-app", "title": "Subtitles"}),
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
                json!({"project": "#sample-app", "title": null}),
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
        assert!(screen.contains("New account for #sample-app: client"), "{screen}");
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
        assert!(render(&mut app, &snap).contains("Role for @gemini-3.8-flash-low-2 in #sample-app"));
        assert_eq!(
            press(&mut app, &snap, &[KeyCode::Down, KeyCode::Enter]),
            Some(Action::CreateAccount {
                body: json!({
                    "template": "agy", "model": "gemini-3.8-flash-low",
                    "nick": "gemini-3.8-flash-low-2", "channel": "#sample-app",
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
            r#"{"default": "user", "extra": 1, "profiles": {"user": {"token": "t", "nick": "user", "server": "s"}}}"#,
        )?;
        save_profile(&dir, "new-model", "http://127.0.0.1:6680", "tok-2")?;
        let data: Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
        assert_eq!(data["default"], "user");
        assert_eq!(data["extra"], 1);
        assert_eq!(data["profiles"]["user"]["token"], "t");
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
        assert!(render(&mut app, &snap).contains("Role for @agent3.8"));
        assert_eq!(
            press(&mut app, &snap, &[KeyCode::Down, KeyCode::Down, KeyCode::Enter]),
            sent(
                "/v1/projects/members",
                json!({"channel": "#sample-app", "nick": "agent3.8", "role": "reviewer"}),
                "@agent3.8 is now reviewer in #sample-app"
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
            "title": "Child", "kind": "task", "channel": "#sample-app", "status": "assigned",
            "assignee": "worker", "reviewer": "lead", "parent_task_id": "task_parent",
        });
        let parent = json!({"title": "Parent work", "assignee": "lead"});
        let lines = task_detail_lines(&body(child.clone()), Some(&parent), Some("agent3.8"));
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
        let top = json!({"title": "Top", "kind": "task", "reviewer": "agent3.8"});
        let lines = task_detail_lines(&body(top), None, Some("agent3.8"));
        assert!(
            lines.contains(&"reviewer @agent3.8 (PM)   parent -".to_string()),
            "{lines:#?}"
        );
        let none = json!({"title": "Unassigned", "kind": "task"});
        let lines = task_detail_lines(&body(none), None, Some("agent3.8"));
        assert!(
            lines.contains(&"reviewer -   parent -".to_string()),
            "{lines:#?}"
        );
    }

    #[test]
    fn only_a_loopback_hub_can_be_started_here() {
        assert_eq!(
            hub_bind_for("http://127.0.0.1:6680").as_deref(),
            Some("127.0.0.1:6680")
        );
        assert_eq!(
            hub_bind_for("http://localhost:7000/").as_deref(),
            Some("127.0.0.1:7000")
        );
        assert_eq!(
            hub_bind_for("http://[::1]:6680").as_deref(),
            Some("[::1]:6680")
        );
        assert_eq!(
            hub_bind_for("http://127.0.0.1").as_deref(),
            Some("127.0.0.1:80")
        );
        assert_eq!(hub_bind_for("http://192.168.1.5:6680"), None);
        assert_eq!(hub_bind_for("http://hub.example.com:6680"), None);
        assert_eq!(hub_bind_for("https://127.0.0.1:6680"), None);
        assert_eq!(hub_bind_for("not a url"), None);
    }

    #[tokio::test]
    async fn a_refused_connection_counts_as_the_hub_being_down() {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let cfg = TuiConfig {
            server: format!("http://127.0.0.1:{port}"),
            token: "t".into(),
            nick: "me".into(),
            owner_key: None,
            ..TuiConfig::default()
        };
        let err = get(&reqwest::Client::new(), &cfg, "/v1/me")
            .await
            .unwrap_err();
        assert!(hub_unreachable(&err), "{err:#}");
        assert!(!hub_unreachable(&anyhow!("unauthorized")));
    }

    #[test]
    fn a_hub_that_is_down_offers_a_key_to_start_it() {
        let down = Snapshot {
            server: "http://127.0.0.1:6680".into(),
            me: "user".into(),
            error: Some("cannot reach the hub at http://127.0.0.1:6680".into()),
            hub_down: true,
            ..Snapshot::default()
        };
        let mut app = home();
        let screen = render(&mut app, &down);
        assert!(
            screen.contains("No hub is running at http://127.0.0.1:6680. [S] start it"),
            "{screen}"
        );
        assert!(
            screen.contains("The hub is not running. Press S to start it"),
            "{screen}"
        );
        assert_eq!(
            press(&mut app, &down, &[KeyCode::Char('S')]),
            Some(Action::StartHub)
        );

        // In chat a capital S is text; Ctrl+S starts the hub instead.
        app.view = View::Chat;
        assert!(render(&mut app, &down).contains("[^S] start it"));
        assert_eq!(press(&mut app, &down, &[KeyCode::Char('S')]), None);
        assert_eq!(app.input, "S");
        assert_eq!(
            handle_key(&mut app, KeyCode::Char('s'), KeyModifiers::CONTROL, &down),
            Some(Action::StartHub)
        );

        // A hub on another machine cannot be started from here.
        let remote = Snapshot {
            server: "http://10.0.0.9:6680".into(),
            ..down.clone()
        };
        let mut app = home();
        assert!(render(&mut app, &remote).contains("Start it on its machine."));
        assert_eq!(press(&mut app, &remote, &[KeyCode::Char('S')]), None);

        // A running hub never gets the offer.
        let mut app = home();
        let up = fixture();
        assert_ne!(
            press(&mut app, &up, &[KeyCode::Char('S')]),
            Some(Action::StartHub)
        );
        assert!(!render(&mut app, &up).contains("No hub is running"));
    }

    #[test]
    fn tui_tls_config_precedence_and_validation() {
        let dir = crate::tls::tests::TempDir::new("aircd-tui-cfg");
        std::fs::write(
            dir.0.join("profiles.json"),
            serde_json::json!({
                "default": "alice",
                "profiles": {
                    "alice": {
                        "server": "https://127.0.0.1:6681/",
                        "token": "tok-1",
                        "nick": "alice",
                        "tls_ca": "/prof/ca.pem",
                        "tls_cert": "/prof/cert.pem",
                        "tls_key": "/prof/key.pem",
                        "tls_insecure": false
                    }
                }
            })
            .to_string(),
        )
        .expect("write profiles");

        // Profile values used when CLI/env are absent.
        let cfg = load_config_from_dir(&dir.0, None, None, TuiTlsOverrides::default(), |_| None)
            .expect("load from profile");
        assert_eq!(cfg.server, "https://127.0.0.1:6681");
        assert_eq!(cfg.tls_ca.as_deref(), Some("/prof/ca.pem"));
        assert_eq!(cfg.tls_cert.as_deref(), Some("/prof/cert.pem"));
        assert_eq!(cfg.tls_key.as_deref(), Some("/prof/key.pem"));
        assert!(!cfg.tls_insecure);

        // Env overrides profile.
        let cfg = load_config_from_dir(&dir.0, None, None, TuiTlsOverrides::default(), |name| {
            match name {
                "AIRCD_TLS_CA" => Some("/env/ca.pem".into()),
                "AIRCD_TLS_INSECURE" => Some("yes".into()),
                _ => None,
            }
        })
        .expect("load with env override");
        assert_eq!(cfg.tls_ca.as_deref(), Some("/env/ca.pem"));
        assert_eq!(cfg.tls_cert.as_deref(), Some("/prof/cert.pem"));
        assert!(cfg.tls_insecure);

        // CLI overrides env and profile.
        let cfg = load_config_from_dir(
            &dir.0,
            None,
            None,
            TuiTlsOverrides {
                ca: Some("/cli/ca.pem".into()),
                cert: Some("/cli/cert.pem".into()),
                key: Some("/cli/key.pem".into()),
                insecure: false,
            },
            |name| match name {
                "AIRCD_TLS_CA" => Some("/env/ca.pem".into()),
                "AIRCD_TLS_CLIENT_CERT" => Some("/env/cert.pem".into()),
                "AIRCD_TLS_CLIENT_KEY" => Some("/env/key.pem".into()),
                _ => None,
            },
        )
        .expect("load with CLI override");
        assert_eq!(cfg.tls_ca.as_deref(), Some("/cli/ca.pem"));
        assert_eq!(cfg.tls_cert.as_deref(), Some("/cli/cert.pem"));
        assert_eq!(cfg.tls_key.as_deref(), Some("/cli/key.pem"));

        // tls_key without tls_cert is rejected.
        std::fs::write(
            dir.0.join("profiles.json"),
            serde_json::json!({
                "default": "bob",
                "profiles": {
                    "bob": {
                        "server": "https://127.0.0.1:6681",
                        "token": "tok-2"
                    }
                }
            })
            .to_string(),
        )
        .expect("write bob profile");
        let err = load_config_from_dir(
            &dir.0,
            None,
            None,
            TuiTlsOverrides {
                key: Some("/only/key.pem".into()),
                ..TuiTlsOverrides::default()
            },
            |_| None,
        )
        .expect_err("key without cert should fail");
        assert!(err.to_string().contains("--tls-cert is required"));
    }

    #[test]
    fn tui_build_http_client_validates_and_loads_mtls() {
        let mut cfg = TuiConfig {
            server: "https://127.0.0.1:6681".into(),
            token: "tok".into(),
            nick: "alice".into(),
            owner_key: None,
            tls_ca: None,
            tls_cert: None,
            tls_key: Some("missing-key.pem".into()),
            tls_insecure: false,
        };
        let err = build_http_client(&cfg).expect_err("key without cert must fail");
        assert!(err.to_string().contains("tls_cert is required"));

        cfg.tls_key = None;
        cfg.tls_ca = Some("/nonexistent/ca.pem".into());
        assert!(build_http_client(&cfg).is_err());

        // Generate real ephemeral CA + client cert via OpenSSL and build reqwest client.
        let pki = crate::tls::tests::TestPki::generate();
        let valid_cfg = TuiConfig {
            server: "https://127.0.0.1:6681".into(),
            token: "tok".into(),
            nick: "alice".into(),
            owner_key: None,
            tls_ca: Some(pki.ca_cert.display().to_string()),
            tls_cert: Some(pki.client_cert.display().to_string()),
            tls_key: Some(pki.client_key.display().to_string()),
            tls_insecure: false,
        };
        assert!(build_http_client(&valid_cfg).is_ok());
    }

    fn groups_fixture() -> Snapshot {
        let mut snap = fixture();
        snap.accounts.extend([
            Account {
                id: "lead".into(),
                nick: "lead".into(),
                role: "leader".into(),
                online: true,
                running: true,
                launcher: Some(true),
                command_found: true,
                last_run: Some("ok".into()),
            },
            Account {
                id: "worker1".into(),
                nick: "worker1".into(),
                role: "builder".into(),
                online: true,
                running: true,
                launcher: Some(true),
                command_found: true,
                last_run: Some("ok".into()),
            },
            Account {
                id: "worker2".into(),
                nick: "worker2".into(),
                role: "builder".into(),
                online: false,
                running: false,
                launcher: Some(true),
                command_found: true,
                last_run: Some("ok".into()),
            },
        ]);
        snap.projects[0].tasks = vec![
            TaskView {
                id: "task_parent".into(),
                kind: "feature".into(),
                priority: "high".into(),
                status: "in_progress".into(),
                assignee: "lead".into(),
                title: "Parent topology epic".into(),
                ..TaskView::default()
            },
            TaskView {
                id: "task_sub1".into(),
                kind: "task".into(),
                priority: "normal".into(),
                status: "in_progress".into(),
                assignee: "worker1".into(),
                title: "Build tree renderer".into(),
                reviewer: Some("lead".into()),
                parent: Some("task_parent".into()),
                ..TaskView::default()
            },
            TaskView {
                id: "task_sub2".into(),
                kind: "bug".into(),
                priority: "urgent".into(),
                status: "review".into(),
                assignee: "worker1".into(),
                title: "Fix scroll clamp".into(),
                reviewer: Some("lead".into()),
                parent: Some("task_parent".into()),
                ..TaskView::default()
            },
            TaskView {
                id: "task_done".into(),
                kind: "task".into(),
                priority: "normal".into(),
                status: "merged".into(),
                assignee: "worker2".into(),
                title: "Completed subtask".into(),
                reviewer: Some("lead".into()),
                parent: Some("task_parent".into()),
                ..TaskView::default()
            },
        ];
        snap.groups = vec![
            GroupView {
                name: "dev".into(),
                description: Some("Opus lead with local workers".into()),
                lead: GroupSeatView {
                    template: "claude".into(),
                    model: "opus".into(),
                    count: 1,
                    nick: Some("lead".into()),
                },
                workers: vec![
                    GroupSeatView {
                        template: "claude".into(),
                        model: "sonnet".into(),
                        count: 1,
                        nick: Some("worker1".into()),
                    },
                    GroupSeatView {
                        template: "codex".into(),
                        model: "gpt-5".into(),
                        count: 1,
                        nick: Some("worker2".into()),
                    },
                    GroupSeatView {
                        template: "ollama".into(),
                        model: "qwen3:35b".into(),
                        count: 3,
                        nick: None,
                    },
                ],
                projects: vec!["#sample-app".into()],
                lead_prompt: Some("Lead custom prompt".into()),
                worker_prompt: Some("Worker custom prompt".into()),
                review_policy: Some(GroupReviewPolicyView {
                    peer_reviewers: vec!["lead".into(), "worker1".into()],
                    quorum: Some(1),
                }),
                inactivity_timeout_secs: Some(300),
            },
            GroupView {
                name: "ops".into(),
                description: None,
                lead: GroupSeatView {
                    template: "gemini".into(),
                    model: "pro".into(),
                    count: 1,
                    nick: Some("ops-lead".into()),
                },
                workers: vec![GroupSeatView {
                    template: "gemini".into(),
                    model: "flash".into(),
                    count: 2,
                    nick: None,
                }],
                projects: Vec::new(),
                lead_prompt: None,
                worker_prompt: None,
                review_policy: None,
                inactivity_timeout_secs: None,
            },
        ];
        snap
    }

    #[test]
    fn groups_panel_renders_topology_lead_active_workers_and_subtasks() {
        let snap = groups_fixture();
        let mut app = App {
            view: View::Groups,
            ..App::default()
        };
        let screen = render(&mut app, &snap);
        for expected in [
            "Groups (2)",
            "dev  #sample-app",
            "@lead · 1/5 active · 2 sub",
            "ops  (unattached)",
            "@ops-lead · 0/2 active · 0 sub",
            "Topology: dev",
            "Opus lead with local workers  ·  projects: #sample-app",
            "policy: quorum 1 (@lead, @worker1)  ·  timeout: 300s",
            "★ @lead  (claude:opus)  coding",
            "[parent] in_progress Parent topology epic",
            "● @worker1  (claude:sonnet)  in review",
            "[sub/task] Build tree renderer",
            "[sub/bug] Fix scroll clamp",
            "○ @worker2  (codex:gpt-5)  ready",
            "○ [pool ×3] ollama:qwen3:35b",
            "Current Subtasks (2)",
            "@worker1",
            "[urgent] [bug] Fix scroll clamp",
            "parent: task_parent \"Parent topology epic\"",
            "review @lead",
        ] {
            assert!(screen.contains(expected), "groups view is missing {expected:?}\n{screen}");
        }
        // Terminal subtasks must not show up as current subtasks.
        assert!(
            !screen.contains("Completed subtask"),
            "terminal subtask should be excluded:\n{screen}"
        );
    }

    #[test]
    fn groups_panel_navigation_focus_click_and_subtask_detail() {
        let mut snap = groups_fixture();
        snap.projects.push(ProjectView {
            channel: "#other".into(),
            name: "Other".into(),
            ..ProjectView::default()
        });
        snap.groups[1].projects = vec!["#other".into()];

        // Pressing 'g' from Dashboard jumps to the group attached to the open project.
        let mut app = App {
            view: View::Dashboard,
            project: Some("#other".into()),
            ..App::default()
        };
        assert_eq!(press(&mut app, &snap, &[KeyCode::Char('g')]), None);
        assert_eq!(app.view, View::Groups);
        assert_eq!(app.group_idx, 1);

        // Moving up selects the first group ("dev") and resets subtask cursor.
        press(&mut app, &snap, &[KeyCode::Up]);
        assert_eq!(app.group_idx, 0);
        assert_eq!(app.groups_focus, GroupsFocus::Groups);

        // Tab switches focus to Current Subtasks; Down moves to the second subtask;
        // Enter opens its task detail modal and dispatches LoadTask.
        press(&mut app, &snap, &[KeyCode::Tab, KeyCode::Down]);
        assert_eq!(app.groups_focus, GroupsFocus::Subtasks);
        assert_eq!(app.group_subtask_cursor, 1);
        assert_eq!(
            press(&mut app, &snap, &[KeyCode::Enter]),
            Some(Action::LoadTask("task_sub2".into()))
        );
        assert_eq!(
            app.modal,
            Some(Modal::Detail {
                task_id: "task_sub2".into()
            })
        );
        press(&mut app, &snap, &[KeyCode::Esc]);
        assert!(app.modal.is_none());

        // Clicking a group row and a subtask row updates selection and focus.
        render(&mut app, &snap);
        let (group2_y, _) = app.group_rows.iter().find(|(_, i)| *i == 1).copied().unwrap();
        handle_click(&mut app, 4, group2_y);
        assert_eq!(app.group_idx, 1);
        assert_eq!(app.groups_focus, GroupsFocus::Groups);

        let (group1_y, _) = app.group_rows.iter().find(|(_, i)| *i == 0).copied().unwrap();
        handle_click(&mut app, 4, group1_y);
        render(&mut app, &snap);
        let (sub1_y, _) = app.group_subtask_rows[0];
        handle_click(&mut app, GROUPS_PANE_WIDTH + 5, sub1_y);
        assert_eq!(app.groups_focus, GroupsFocus::Subtasks);
        assert_eq!(app.group_subtask_cursor, 0);

        // 'c' opens Chat in the selected group's attached project room.
        press(&mut app, &snap, &[KeyCode::Char('c')]);
        assert_eq!(app.view, View::Chat);
        assert_eq!(chat_rooms(&app, &snap)[app.channel_idx], "#sample-app");

        // Clicking the [G]roups tab switches back to Groups; 'd' returns to Dashboard.
        let groups_x = TABS.find("[G]roups").unwrap() as u16 + 1;
        handle_click(&mut app, groups_x, 0);
        assert_eq!(app.view, View::Groups);
        press(&mut app, &snap, &[KeyCode::Char('d')]);
        assert_eq!(app.view, View::Dashboard);
    }

    #[test]
    fn groups_panel_handles_empty_groups_and_small_terminal_sizes() {
        let empty_snap = fixture();
        let mut app = App {
            view: View::Groups,
            ..App::default()
        };
        let screen = render(&mut app, &empty_snap);
        assert!(screen.contains("No groups configured yet."), "{screen}");
        assert!(screen.contains("No active subtasks."), "{screen}");

        let populated = groups_fixture();
        for (width, height) in [(110, 32), (60, 10), (20, 6), (1, 1)] {
            render_to_text(&mut app, &populated, width, height).unwrap();
        }
    }

    #[test]
    fn group_view_parses_http_json_and_handles_missing_fields() {
        let parsed = group_view(&json!({
            "name": "dev",
            "description": "Opus lead with local workers",
            "lead": {"template": "claude", "model": "opus", "count": 1, "nick": "@Lead"},
            "workers": [
                {"template": "claude", "model": "sonnet", "nick": "worker1"},
                {"template": "ollama", "model": "qwen3:35b", "count": 3}
            ],
            "review_policy": {"peer_reviewers": ["@Lead", "worker1"], "quorum": 1},
            "inactivity_timeout_secs": 300,
            "projects": ["#cams"]
        }));
        assert_eq!(parsed.name, "dev");
        assert_eq!(parsed.lead.nick.as_deref(), Some("Lead"));
        assert_eq!(parsed.workers.len(), 2);
        assert_eq!(parsed.workers[0].count, 1);
        assert_eq!(parsed.workers[1].count, 3);
        assert_eq!(parsed.workers[1].nick, None);
        assert_eq!(parsed.projects, vec!["#cams"]);
        assert_eq!(
            parsed.review_policy,
            Some(GroupReviewPolicyView {
                peer_reviewers: vec!["Lead".into(), "worker1".into()],
                quorum: Some(1),
            })
        );
        assert_eq!(parsed.inactivity_timeout_secs, Some(300));
        assert_eq!(parsed.lead_prompt, None);
        assert_eq!(parsed.worker_prompt, None);
    }

    #[test]
    fn group_editor_creates_new_group_with_lead_and_workers() {
        let mut snap = fixture();
        snap.templates = vec![TemplateView {
            id: "claude".into(),
            name: "Claude Code".into(),
            installed: true,
        }];
        snap.models = Some((
            "claude".into(),
            Ok(vec![
                ("opus".into(), Some("Opus 4".into())),
                ("sonnet".into(), Some("Sonnet 4".into())),
            ]),
        ));
        let mut app = App {
            view: View::Groups,
            ..App::default()
        };

        // Pressing 'n' even when no groups exist opens the interactive Group Editor modal.
        assert_eq!(press(&mut app, &snap, &[KeyCode::Char('n')]), None);
        let screen = render(&mut app, &snap);
        assert!(screen.contains("New group"), "{screen}");
        assert!(screen.contains("[ Save Group ]"), "{screen}");

        // Fill Name ("backend").
        press(&mut app, &snap, &[KeyCode::Enter]);
        type_text(&mut app, &snap, "backend");
        press(&mut app, &snap, &[KeyCode::Enter]);

        // Move to Description and fill it.
        press(&mut app, &snap, &[KeyCode::Down, KeyCode::Enter]);
        type_text(&mut app, &snap, "Core team");
        press(&mut app, &snap, &[KeyCode::Enter]);

        // Move to Lead, open template picker, select "claude" -> dispatches LoadModels("claude").
        press(&mut app, &snap, &[KeyCode::Down, KeyCode::Enter]);
        let screen = render(&mut app, &snap);
        assert!(screen.contains("Lead seat: client template"), "{screen}");
        assert_eq!(
            press(&mut app, &snap, &[KeyCode::Enter]),
            Some(Action::LoadModels("claude".into()))
        );

        // Pick model "opus" (index 0) -> opens nick picker.
        let screen = render(&mut app, &snap);
        assert!(screen.contains("Lead seat: model for claude"), "{screen}");
        press(&mut app, &snap, &[KeyCode::Enter]);

        // Pick account nick "@agent3.8" (index 2: after "(pool / unnamed)" and "@user").
        press(&mut app, &snap, &[KeyCode::Down, KeyCode::Down, KeyCode::Enter]);

        // Move to "+ Add worker seat" and add a manual pool worker ("ollama", "qwen3:35b", count 3).
        press(&mut app, &snap, &[KeyCode::Down, KeyCode::Enter]);
        // Template picker has "Claude Code" (0) and "Type a template id..." (1).
        press(&mut app, &snap, &[KeyCode::Down, KeyCode::Enter]);
        type_text(&mut app, &snap, "ollama");
        assert_eq!(
            press(&mut app, &snap, &[KeyCode::Enter]),
            Some(Action::LoadModels("ollama".into()))
        );
        // Model picker for "ollama" is loading; "Type a model name..." is index 0.
        press(&mut app, &snap, &[KeyCode::Enter]);
        type_text(&mut app, &snap, "qwen3:35b");
        press(&mut app, &snap, &[KeyCode::Enter]);
        // Count prompt is pre-filled with "1"; backspace and type "3".
        press(&mut app, &snap, &[KeyCode::Backspace]);
        type_text(&mut app, &snap, "3");
        press(&mut app, &snap, &[KeyCode::Enter]);
        // Nick picker: index 0 is "(pool / unnamed)".
        press(&mut app, &snap, &[KeyCode::Enter]);

        // Move to Timeout (s) and set "600".
        press(&mut app, &snap, &[KeyCode::Down, KeyCode::Enter]);
        type_text(&mut app, &snap, "600");
        press(&mut app, &snap, &[KeyCode::Enter]);

        // Press 's' to save.
        let action = press(&mut app, &snap, &[KeyCode::Char('s')]);
        assert!(app.modal.is_none());
        assert_eq!(
            action,
            Some(Action::Api {
                path: "/v1/groups".into(),
                body: json!({
                    "name": "backend",
                    "description": "Core team",
                    "lead": {"template": "claude", "model": "opus", "count": 1, "nick": "agent3.8"},
                    "workers": [
                        {"template": "ollama", "model": "qwen3:35b", "count": 3}
                    ],
                    "lead_prompt": Value::Null,
                    "worker_prompt": Value::Null,
                    "review_policy": Value::Null,
                    "inactivity_timeout_secs": 600,
                }),
                owner: true,
                done: "Group 'backend' created".into(),
            })
        );
    }

    #[test]
    fn group_editor_edits_existing_group_preserving_prompts_and_adjusting_workers() {
        let snap = groups_fixture();
        let mut app = App {
            view: View::Groups,
            group_idx: 0,
            ..App::default()
        };

        // Pressing 'e' opens GroupEditor pre-populated with "dev".
        assert_eq!(press(&mut app, &snap, &[KeyCode::Char('e')]), None);
        let screen = render(&mut app, &snap);
        assert!(screen.contains("Edit group: dev"), "{screen}");
        assert!(screen.contains("dev  (locked)"), "{screen}");
        assert!(screen.contains("Opus lead with local workers"), "{screen}");

        // Trying to edit Name shows a locked message and keeps the modal open.
        press(&mut app, &snap, &[KeyCode::Up, KeyCode::Enter]);
        let screen = render(&mut app, &snap);
        assert!(screen.contains("cannot be renamed"), "{screen}");

        // Move to Worker 2 (codex:gpt-5 @worker2) and delete it with 'd'.
        press(
            &mut app,
            &snap,
            &[KeyCode::Down, KeyCode::Down, KeyCode::Down, KeyCode::Down, KeyCode::Char('d')],
        );
        // Now Worker 2 is ollama:qwen3:35b ×3; increment its count with '+' to ×4.
        press(&mut app, &snap, &[KeyCode::Char('+')]);
        // Delete Worker 2 as well, leaving only Worker 1.
        press(&mut app, &snap, &[KeyCode::Char('d')]);
        // Trying to delete the last remaining worker is refused.
        press(&mut app, &snap, &[KeyCode::Up, KeyCode::Char('d')]);
        let screen = render(&mut app, &snap);
        assert!(screen.contains("at least one worker"), "{screen}");

        // Edit Worker 1 via SeatStep::Menu: change model to "haiku" and commit with "[ Done ]".
        press(&mut app, &snap, &[KeyCode::Enter]);
        let screen = render(&mut app, &snap);
        assert!(screen.contains("Worker seat 1"), "{screen}");
        // Select Model row (index 1) -> PickModel -> TypeModel (last row).
        assert_eq!(
            press(&mut app, &snap, &[KeyCode::Down, KeyCode::Enter]),
            Some(Action::LoadModels("claude".into()))
        );
        press(&mut app, &snap, &[KeyCode::Down, KeyCode::Enter]);
        press(&mut app, &snap, &[KeyCode::Backspace; 6]);
        type_text(&mut app, &snap, "haiku");
        press(&mut app, &snap, &[KeyCode::Enter]);
        // Back in Menu: move to [ Done ] (row 4) and confirm.
        press(
            &mut app,
            &snap,
            &[
                KeyCode::Down,
                KeyCode::Down,
                KeyCode::Down,
                KeyCode::Down,
                KeyCode::Enter,
            ],
        );

        // Save via Ctrl+S and verify existing prompts and review_policy are preserved.
        let action = handle_key(
            &mut app,
            KeyCode::Char('s'),
            KeyModifiers::CONTROL,
            &snap,
        );
        assert!(app.modal.is_none());
        assert_eq!(
            action,
            Some(Action::Api {
                path: "/v1/groups".into(),
                body: json!({
                    "name": "dev",
                    "description": "Opus lead with local workers",
                    "lead": {"template": "claude", "model": "opus", "count": 1, "nick": "lead"},
                    "workers": [
                        {"template": "claude", "model": "haiku", "count": 1, "nick": "worker1"}
                    ],
                    "lead_prompt": "Lead custom prompt",
                    "worker_prompt": "Worker custom prompt",
                    "review_policy": {"peer_reviewers": ["lead", "worker1"], "quorum": 1},
                    "inactivity_timeout_secs": 300,
                }),
                owner: true,
                done: "Group 'dev' updated".into(),
            })
        );
    }

    #[test]
    fn group_editor_validates_invalid_inputs_and_renders_small_terminals() {
        let snap = groups_fixture();
        let mut editor = GroupEditorState::new_group();
        assert!(editor.build_upsert_payload().is_err());

        editor.name = "bad/name".into();
        assert!(editor.build_upsert_payload().is_err());

        editor.name = "team".into();
        editor.lead = GroupEditorSeat {
            template: "claude".into(),
            model: "opus".into(),
            count: 1,
            nick: Some("lead".into()),
        };
        editor.workers.push(GroupEditorSeat {
            template: "claude".into(),
            model: "sonnet".into(),
            count: 1,
            nick: Some("worker1".into()),
        });
        editor.timeout_secs = "0".into();
        assert!(editor.build_upsert_payload().is_err());

        editor.timeout_secs = "120".into();
        editor.review_peers = "lead, unknown".into();
        assert!(editor.build_upsert_payload().is_err());

        editor.review_peers = "lead, worker1".into();
        editor.review_quorum = "3".into();
        assert!(editor.build_upsert_payload().is_err());

        editor.review_quorum = "2".into();
        assert!(editor.build_upsert_payload().is_ok());

        let mut app = App {
            view: View::Groups,
            modal: Some(Modal::GroupEditor(Box::new(editor))),
            ..App::default()
        };
        for (width, height) in [(110, 32), (60, 10), (20, 6), (1, 1)] {
            render_to_text(&mut app, &snap, width, height).unwrap();
        }
    }

    #[test]
    fn group_attach_dialog_attaches_single_and_multiple_projects() {
        let mut snap = groups_fixture();
        snap.projects.push(ProjectView {
            channel: "#cams".into(),
            name: "CAMS".into(),
            pm: Some("flash".into()),
            ..ProjectView::default()
        });
        snap.projects.push(ProjectView {
            channel: "#infra".into(),
            name: "Infra".into(),
            ..ProjectView::default()
        });
        snap.groups[0].name = "dev équipe".into();

        let mut app = App {
            view: View::Groups,
            group_idx: 0,
            project: Some("#cams".into()),
            ..App::default()
        };

        // Pressing 'a' opens the Project Attachment dialog, pre-highlighting app.project ("#cams").
        assert_eq!(press(&mut app, &snap, &[KeyCode::Char('a')]), None);
        let screen = render(&mut app, &snap);
        assert!(screen.contains("Attach group 'dev équipe' to project"), "{screen}");
        assert!(screen.contains("[attached] #sample-app"), "{screen}");
        assert!(screen.contains("[        ] #cams"), "{screen}");

        // Enter attaches the highlighted project directly with URL-encoded group name.
        let action = press(&mut app, &snap, &[KeyCode::Enter]);
        assert!(app.modal.is_none());
        assert_eq!(
            action,
            Some(Action::Api {
                path: "/v1/groups/dev%20%C3%A9quipe/attach".into(),
                body: json!({"channel": "#cams"}),
                owner: true,
                done: "Attached group 'dev équipe' to #cams".into(),
            })
        );

        // Multi-select with Space attaches multiple projects via Action::Many.
        press(&mut app, &snap, &[KeyCode::Char('a')]);
        press(
            &mut app,
            &snap,
            &[KeyCode::Char(' '), KeyCode::Down, KeyCode::Char(' ')],
        );
        let screen = render(&mut app, &snap);
        assert!(screen.contains("[selected] #cams"), "{screen}");
        assert!(screen.contains("[selected] #infra"), "{screen}");
        let multi = press(&mut app, &snap, &[KeyCode::Enter]);
        assert_eq!(
            multi,
            Some(Action::Many(vec![
                Action::Api {
                    path: "/v1/groups/dev%20%C3%A9quipe/attach".into(),
                    body: json!({"channel": "#cams"}),
                    owner: true,
                    done: "Attached group 'dev équipe' to 2 project(s)".into(),
                },
                Action::Api {
                    path: "/v1/groups/dev%20%C3%A9quipe/attach".into(),
                    body: json!({"channel": "#infra"}),
                    owner: true,
                    done: "Attached group 'dev équipe' to 2 project(s)".into(),
                },
            ]))
        );

        // Empty projects snapshot shows a clear message and closes on Esc.
        snap.projects.clear();
        press(&mut app, &snap, &[KeyCode::Char('a')]);
        let screen = render(&mut app, &snap);
        assert!(screen.contains("No projects exist on the hub yet."), "{screen}");
        press(&mut app, &snap, &[KeyCode::Esc]);
        assert!(app.modal.is_none());
    }

    #[test]
    fn search_modal_searches_sprint_channels_and_dms_and_cycles_scopes() {
        let mut snap = fixture();
        snap.channels.push("@agent3.8".into());
        snap.messages.insert(
            "#general".into(),
            vec![ChatLine {
                seq: 10,
                sender: "user".into(),
                body: "Hub deployment notes".into(),
                time: 3600 * 9,
            }],
        );
        snap.messages.insert(
            "#sample-app-s1".into(),
            vec![
                ChatLine {
                    seq: 20,
                    sender: "codex".into(),
                    body: "Sprint deploy script updated".into(),
                    time: 3600 * 10,
                },
                ChatLine {
                    seq: 21,
                    sender: "agent3.8".into(),
                    body: "Sprint review passed".into(),
                    time: 3600 * 11,
                },
            ],
        );
        snap.messages.insert(
            "@agent3.8".into(),
            vec![ChatLine {
                seq: 30,
                sender: "agent3.8".into(),
                body: "DM: ready to deploy seek bar".into(),
                time: 3600 * 12,
            }],
        );

        let mut app = App {
            view: View::Chat,
            channel_idx: 2, // #sample-app-s1
            ..App::default()
        };

        // Ctrl+F opens Search modal from Chat (preserving current_room).
        assert_eq!(
            handle_key(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL, &snap),
            None
        );
        let empty_screen = render(&mut app, &snap);
        assert!(empty_screen.contains("Search Chat History"), "{empty_screen}");
        assert!(empty_screen.contains("1 sprint channel(s), 1 DM thread(s)"), "{empty_screen}");

        // Typing "deploy" searches All rooms (DM, sprint, and #general) newest first,
        // and emits Action::SearchHistory.
        let action = press(
            &mut app,
            &snap,
            &[
                KeyCode::Char('d'),
                KeyCode::Char('e'),
                KeyCode::Char('p'),
                KeyCode::Char('l'),
                KeyCode::Char('o'),
                KeyCode::Char('y'),
            ],
        );
        assert_eq!(
            action,
            Some(Action::SearchHistory {
                query: "deploy".into(),
                scope: SearchScope::All,
                channel: None,
            })
        );
        let all_screen = render(&mut app, &snap);
        assert!(all_screen.contains("3 match(es) [1/3]"), "{all_screen}");
        assert!(all_screen.contains("@agent3.8"), "{all_screen}");
        assert!(all_screen.contains("#sample-app-s1"), "{all_screen}");
        assert!(all_screen.contains("#general"), "{all_screen}");

        // Tab -> Sprints scope: only #sample-app-s1 matches.
        assert_eq!(
            press(&mut app, &snap, &[KeyCode::Tab]),
            Some(Action::SearchHistory {
                query: "deploy".into(),
                scope: SearchScope::Sprints,
                channel: None,
            })
        );
        let sprint_screen = render(&mut app, &snap);
        assert!(sprint_screen.contains("1 match(es) [1/1]"), "{sprint_screen}");
        assert!(sprint_screen.contains("Sprint deploy script updated"), "{sprint_screen}");

        // Tab -> DMs scope: only @agent3.8 DM matches.
        assert_eq!(
            press(&mut app, &snap, &[KeyCode::Tab]),
            Some(Action::SearchHistory {
                query: "deploy".into(),
                scope: SearchScope::DirectMessages,
                channel: None,
            })
        );
        let dm_screen = render(&mut app, &snap);
        assert!(dm_screen.contains("1 match(es) [1/1]"), "{dm_screen}");
        assert!(dm_screen.contains("DM: ready to deploy seek bar"), "{dm_screen}");

        // Tab -> Room scope (#sample-app-s1).
        assert_eq!(
            press(&mut app, &snap, &[KeyCode::Tab]),
            Some(Action::SearchHistory {
                query: "deploy".into(),
                scope: SearchScope::CurrentRoom,
                channel: Some("#sample-app-s1".into()),
            })
        );
        let room_screen = render(&mut app, &snap);
        assert!(room_screen.contains("Room: #sample-app-s1"), "{room_screen}");
        assert!(room_screen.contains("1 match(es) [1/1]"), "{room_screen}");

        // BackTab returns to DMs, Ctrl+U clears the query, Esc closes the modal.
        press(&mut app, &snap, &[KeyCode::BackTab]);
        handle_key(&mut app, KeyCode::Char('u'), KeyModifiers::CONTROL, &snap);
        assert!(render(&mut app, &snap).contains("Type keywords to search"));

        // Qualifiers from: and in: work in All scope.
        press(&mut app, &snap, &[KeyCode::BackTab, KeyCode::BackTab]); // Back to All
        type_text(&mut app, &snap, "from:codex in:s1 script");
        let qual_screen = render(&mut app, &snap);
        assert!(qual_screen.contains("1 match(es) [1/1]"), "{qual_screen}");
        assert!(qual_screen.contains("Sprint deploy script updated"), "{qual_screen}");

        press(&mut app, &snap, &[KeyCode::Esc]);
        assert!(app.modal.is_none());

        // '/' opens search modal from Dashboard as well.
        app.view = View::Dashboard;
        press(&mut app, &snap, &[KeyCode::Char('/')]);
        assert!(matches!(app.modal, Some(Modal::Search(_))));
    }

    #[test]
    fn search_modal_jumps_to_matched_message_in_chat_and_loads_older_window() {
        let (mut app, mut snap) = chat_with_history(60);
        // Add a sprint room with an older matched message and many newer messages.
        let sprint_msgs: Vec<ChatLine> = (1..=80)
            .map(|seq| ChatLine {
                seq,
                sender: if seq == 15 { "agent3.8" } else { "codex" }.into(),
                body: if seq == 15 {
                    "Found the critical seekbar regression here".into()
                } else {
                    format!("sprint-chatter-{seq:03}")
                },
                time: 3600 + seq * 60,
            })
            .collect();
        snap.messages.insert("#sample-app-s1".into(), sprint_msgs);
        // Start in #general (index 0) and open search via Ctrl+F.
        app.channel_idx = 0;
        handle_key(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL, &snap);
        type_text(&mut app, &snap, "seekbar regression");
        assert_eq!(press(&mut app, &snap, &[KeyCode::Enter]), None);
        assert!(app.modal.is_none());
        assert_eq!(app.view, View::Chat);

        // Rendering Chat jumps directly to #sample-app-s1 and scrolls to seq #15 with highlight.
        let chat_screen = render(&mut app, &snap);
        assert_eq!(chat_rooms(&app, &snap)[app.channel_idx], "#sample-app-s1");
        assert!(chat_screen.contains("Found the critical seekbar regression"), "{chat_screen}");
        assert!(chat_screen.contains("» "), "{chat_screen}");
        assert!(app.chat_scroll > 0);

        // Pressing End jumps to latest and clears the search highlight.
        press(&mut app, &snap, &[KeyCode::End]);
        assert_eq!(app.chat_scroll, 0);
        assert_eq!(app.chat_highlight, None);

        // Server search result not in snap.messages yet merges cleanly and requests LoadMessageWindow on Enter.
        snap.search_results = Some(SearchResponse {
            query: "ancient".into(),
            scope: SearchScope::All,
            channel: None,
            hits: vec![
                SearchHit {
                    room: "#sample-app-s1".into(),
                    seq: 2,
                    sender: "codex".into(),
                    body: "sprint-chatter-002".into(), // already in snap.messages (dedup test)
                    time: 0,
                    is_sprint: true,
                    is_dm: false,
                },
                SearchHit {
                    room: "#sample-app".into(),
                    seq: 1,
                    sender: "agent3.8".into(),
                    body: "An ancient archived decision".into(),
                    time: 3600,
                    is_sprint: false,
                    is_dm: false,
                },
            ],
            error: None,
        });
        // Wait: #sample-app in chat_with_history has seq 1..=60; use seq 9999 not in snap.messages!
        snap.search_results.as_mut().unwrap().hits[1].seq = 9999;
        handle_key(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL, &snap);
        type_text(&mut app, &snap, "ancient");
        let jump_action = press(&mut app, &snap, &[KeyCode::Enter]);
        assert_eq!(
            jump_action,
            Some(Action::LoadMessageWindow {
                room: "#sample-app".into(),
                around_seq: 9999,
            })
        );
    }

    #[test]
    fn search_modal_renders_safely_at_small_terminal_sizes() {
        let snap = fixture();
        let mut app = App {
            view: View::Chat,
            modal: Some(Modal::Search(Box::new(SearchState {
                query: "Stepping".into(),
                scope: SearchScope::All,
                current_room: Some("#sample-app".into()),
                idx: 0,
            }))),
            ..App::default()
        };
        for (width, height) in [(110, 32), (60, 10), (20, 6), (1, 1)] {
            render_to_text(&mut app, &snap, width, height).unwrap();
        }
    }
}
