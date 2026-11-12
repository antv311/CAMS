use anyhow::{Context, Result};
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::net::{IpAddr, UdpSocket};
use tokio::net::TcpListener;

use crate::auth::{DuplicatePrincipalError, Principal};
use crate::gits::{sanitize_repo_name, verify_github_signature};
use crate::locks::LockConflictError;
use crate::server::Server;

#[derive(Clone, Debug, Serialize)]
pub struct HttpRuntimeInfo {
    pub irc_bind: String,
    pub irc_port: u16,
    pub http_bind: String,
    pub http_port: u16,
    pub tls_port: Option<u16>,
    pub detected_hosts: Vec<String>,
    /// Owner invite key. When set, `POST /v1/register` must present it.
    #[serde(skip)]
    pub register_key: Option<String>,
}

#[derive(Clone)]
struct AppState {
    server: Server,
    info: HttpRuntimeInfo,
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, message)
    }

    fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, message)
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    fn internal(err: anyhow::Error) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            with_cors_headers(),
            Json(json!({ "ok": false, "error": self.message })),
        )
            .into_response()
    }
}

pub fn build_runtime_info(
    irc_bind: &str,
    http_bind: &str,
    tls_port: Option<u16>,
) -> HttpRuntimeInfo {
    HttpRuntimeInfo {
        irc_bind: irc_bind.to_string(),
        irc_port: parse_port(irc_bind, 6667),
        http_bind: http_bind.to_string(),
        http_port: parse_port(http_bind, 6680),
        tls_port,
        detected_hosts: reachable_hosts(http_bind),
        register_key: None,
    }
}

/// Addresses the HTTP listener can actually be reached on. A wildcard bind is
/// reachable on every local address; a specific bind only on that host.
fn reachable_hosts(http_bind: &str) -> Vec<String> {
    let host = http_bind
        .rsplit_once(':')
        .map(|(host, _)| host)
        .unwrap_or(http_bind)
        .trim_matches(|c| c == '[' || c == ']');
    match host {
        "" | "0.0.0.0" | "::" => detect_local_ips(),
        specific => vec![specific.to_string()],
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn detect_local_ips() -> Vec<String> {
    let mut hosts = BTreeSet::new();
    hosts.insert("127.0.0.1".to_string());

    if let Ok(sock) = UdpSocket::bind("0.0.0.0:0") {
        if sock.connect("8.8.8.8:80").is_ok() {
            if let Ok(local) = sock.local_addr() {
                let ip = local.ip();
                if !ip.is_unspecified() && !ip.is_loopback() {
                    hosts.insert(ip.to_string());
                }
            }
        }
    }

    if let Ok(host) = std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")) {
        if let Ok(addrs) = std::net::ToSocketAddrs::to_socket_addrs(&(host.as_str(), 0)) {
            for addr in addrs {
                let ip = addr.ip();
                if let IpAddr::V4(v4) = ip {
                    if !v4.is_unspecified() && !v4.is_loopback() {
                        hosts.insert(v4.to_string());
                    }
                }
            }
        }
    }

    hosts.into_iter().collect()
}

pub async fn serve(server: Server, bind_addr: &str, info: HttpRuntimeInfo) -> Result<()> {
    let listener = TcpListener::bind(bind_addr)
        .await
        .with_context(|| format!("bind HTTP welcome/API listener on {bind_addr}"))?;
    let actual_port = listener.local_addr()?.port();
    let mut runtime = info;
    runtime.http_port = actual_port;
    println!("aircd HTTP welcome & API listening on http://{bind_addr} (port {actual_port})");
    axum::serve(listener, router(server, runtime))
        .await
        .context("serve HTTP router")
}

pub fn router(server: Server, info: HttpRuntimeInfo) -> Router {
    let state = AppState { server, info };
    Router::new()
        .route("/", get(welcome_root))
        .route("/welcome", get(welcome_root))
        .route("/instructions", get(welcome_root))
        .route("/v1/welcome", get(welcome_json))
        .route("/healthz", get(healthz))
        .route("/v1/agents", get(public_agents))
        .route("/v1/channels", get(public_channels))
        .route("/v1/gits", get(public_gits).post(register_git_repo))
        .route("/v1/locks", get(public_locks).post(acquire_lock))
        .route("/v1/locks/release", post(release_lock_post))
        .route("/v1/locks/{resource}", delete(release_lock_delete))
        .route("/v1/register", post(register_agent))
        .route("/v1/owner/reissue", post(owner_reissue_token))
        .route("/v1/owner/wake", post(owner_wake))
        .route("/v1/login", post(login_agent))
        .route("/v1/me", get(get_me))
        .route("/v1/channels/join", post(join_channels))
        .route("/v1/messages", get(list_messages).post(post_message))
        .route("/v1/roles", get(list_roles).post(set_role))
        .route(
            "/v1/webhooks",
            get(get_webhook).post(set_webhook).delete(delete_webhook),
        )
        .route("/v1/webhooks/test", post(test_webhook))
        .route("/v1/webhooks/deliveries", get(list_webhook_deliveries))
        .route("/v1/reports", get(list_reports).post(create_report))
        .route("/v1/reports/{id}", get(get_report))
        .route("/v1/tasks", get(list_tasks).post(create_task))
        .route("/v1/tasks/{id}/claim", post(claim_task))
        .route("/v1/tasks/{id}/done", post(done_task))
        .route("/v1/tasks/{id}/release", post(release_task))
        .route("/v1/tasks/{id}", get(get_task))
        .route("/v1/tasks/{id}/assign", post(assign_task))
        .route("/v1/tasks/{id}/status", post(set_task_status))
        .route("/v1/tasks/{id}/ask", post(ask_task))
        .route("/v1/tasks/{id}/answer", post(answer_task))
        .route("/v1/tasks/{id}/escalate", post(escalate_task))
        .route("/v1/notes", get(get_notes).post(write_notes))
        .route("/v1/projects/pm", post(set_project_pm))
        .route("/v1/inbox", get(get_inbox))
        .route("/v1/projects", get(list_projects).post(create_project))
        .route(
            "/v1/projects/members",
            get(list_project_members).post(set_project_member),
        )
        .route("/v1/git/event", post(post_git_event))
        .route("/v1/git/webhook", post(github_webhook_query))
        .route("/v1/git/webhook/{repo}", post(github_webhook_repo))
        .fallback(cors_preflight_or_404)
        .with_state(state)
}

async fn cors_preflight_or_404(method: Method) -> Response {
    if method == Method::OPTIONS {
        return (StatusCode::NO_CONTENT, with_cors_headers(), "").into_response();
    }
    (
        StatusCode::NOT_FOUND,
        with_cors_headers(),
        Json(json!({ "ok": false, "error": "Not found; visit / or /v1/welcome for onboarding instructions." })),
    )
        .into_response()
}

#[derive(Deserialize)]
struct WelcomeQuery {
    format: Option<String>,
}

async fn welcome_root(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<WelcomeQuery>,
) -> Result<Response, ApiError> {
    let base_host = resolve_request_host(&headers, &state.info);
    let fmt = query
        .format
        .as_deref()
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();

    if fmt == "json" || (fmt.is_empty() && prefers_json(&headers)) {
        let payload = build_welcome_payload(&state, &base_host)?;
        return Ok((StatusCode::OK, with_cors_headers(), Json(payload)).into_response());
    }
    if fmt == "md" || fmt == "markdown" || fmt == "text" {
        let md = build_welcome_markdown(&state, &base_host)?;
        return Ok((
            StatusCode::OK,
            with_cors_headers(),
            [(header::CONTENT_TYPE, "text/markdown; charset=utf-8")],
            md,
        )
            .into_response());
    }

    let html = build_welcome_html(&state, &base_host)?;
    Ok((StatusCode::OK, with_cors_headers(), Html(html)).into_response())
}

async fn welcome_json(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    let base_host = resolve_request_host(&headers, &state.info);
    let payload = build_welcome_payload(&state, &base_host)?;
    Ok((StatusCode::OK, with_cors_headers(), Json(payload)))
}

async fn healthz(State(state): State<AppState>) -> impl IntoResponse {
    (
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({
            "ok": true,
            "service": "aircd",
            "irc_port": state.info.irc_port,
            "http_port": state.info.http_port,
            "tls_port": state.info.tls_port,
        })),
    )
}

async fn public_agents(State(state): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    let snapshot = state
        .server
        .directory_snapshot()
        .map_err(ApiError::internal)?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({ "ok": true, "agents": snapshot.agents })),
    ))
}

async fn public_channels(State(state): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    let snapshot = state
        .server
        .directory_snapshot()
        .map_err(ApiError::internal)?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({ "ok": true, "channels": snapshot.channels })),
    ))
}

async fn public_gits(State(state): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    let snapshot = state
        .server
        .directory_snapshot()
        .map_err(ApiError::internal)?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({ "ok": true, "git_repos": snapshot.git_repos })),
    ))
}

async fn public_locks(
    State(state): State<AppState>,
    Query(q): Query<ChannelQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let ch = q.channel.as_deref().map(normalize_http_channel);
    let locks = state
        .server
        .locks()
        .list(ch.as_deref())
        .map_err(ApiError::internal)?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({ "ok": true, "locks": locks })),
    ))
}

#[derive(Deserialize)]
struct RegisterReq {
    nick: String,
    role: Option<String>,
    team: Option<String>,
    channels: Option<Vec<String>>,
    invite: Option<String>,
}

async fn register_agent(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<RegisterReq>,
) -> Result<impl IntoResponse, ApiError> {
    if let Some(expected) = state.info.register_key.as_deref() {
        let supplied = headers
            .get("x-aircd-invite")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
            .or_else(|| req.invite.clone())
            .unwrap_or_default();
        if !constant_time_eq(supplied.trim().as_bytes(), expected.as_bytes()) {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "registration requires the owner's invite key: send it as \"invite\" in the JSON body or in the X-AirCD-Invite header",
            ));
        }
    }
    let (principal, token) = state
        .server
        .auth()
        .register_principal(&req.nick, req.role.as_deref(), req.team.as_deref())
        .map_err(|err| {
            if err.downcast_ref::<DuplicatePrincipalError>().is_some() {
                ApiError::conflict(err.to_string())
            } else {
                ApiError::bad_request(err.to_string())
            }
        })?;

    let initial_channels = req
        .channels
        .filter(|list| !list.is_empty())
        .unwrap_or_else(|| vec!["#general".to_string()]);
    let joined = state
        .server
        .join_channels(&principal, &initial_channels)
        .map_err(ApiError::internal)?;

    let host = resolve_request_host(&headers, &state.info);
    Ok((
        StatusCode::CREATED,
        with_cors_headers(),
        Json(json!({
            "ok": true,
            "principal": principal,
            "token": token,
            "irc_host": host,
            "irc_port": state.info.irc_port,
            "http_url": format!("http://{}:{}", host, state.info.http_port),
            "channels": joined,
        })),
    ))
}

/// Check the owner key from the `X-AirCD-Invite` header or the request body.
fn require_owner(
    state: &AppState,
    headers: &HeaderMap,
    body_key: Option<&str>,
) -> Result<(), ApiError> {
    let Some(expected) = state.info.register_key.as_deref() else {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "owner actions need an owner key to be configured: start aircd without --open-registration",
        ));
    };
    let supplied = headers
        .get("x-aircd-invite")
        .and_then(|v| v.to_str().ok())
        .or(body_key)
        .unwrap_or_default();
    if constant_time_eq(supplied.trim().as_bytes(), expected.as_bytes()) {
        Ok(())
    } else {
        Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "this action requires the owner's key: send it as \"invite\" in the JSON body or in the X-AirCD-Invite header",
        ))
    }
}

#[derive(Deserialize)]
struct OwnerWakeReq {
    invite: Option<String>,
    /// `set` (default), `show`, `delete`, `runs`, `test` or `reset`.
    action: Option<String>,
    nick: Option<String>,
    limit: Option<i64>,
    /// For `test`: the reason handed to the agent, such as an instruction.
    message: Option<String>,
    /// For `test`: launch the account in this project channel.
    channel: Option<String>,
    #[serde(flatten)]
    config: crate::wake::WakeConfigInput,
}

/// Owner-only: configure how the server starts an account's agent. It is
/// owner-only because the configured command runs on the server's machine.
async fn owner_wake(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<OwnerWakeReq>,
) -> Result<impl IntoResponse, ApiError> {
    require_owner(&state, &headers, req.invite.as_deref())?;
    let action = req.action.as_deref().unwrap_or("set").to_ascii_lowercase();
    let wakes = state.server.wakes();

    if action == "runs" && req.nick.is_none() {
        let runs = wakes
            .runs(None, req.limit.unwrap_or(20))
            .map_err(ApiError::internal)?;
        return Ok((
            StatusCode::OK,
            with_cors_headers(),
            Json(json!({ "ok": true, "runs": runs })),
        ));
    }

    let nick = req
        .nick
        .as_deref()
        .ok_or_else(|| ApiError::bad_request("nick is required"))?;
    let principal = state
        .server
        .auth()
        .resolve_by_name(nick)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(format!("no agent named '{}'", nick.trim())))?;

    let body = match action.as_str() {
        "set" => {
            let cfg = wakes
                .upsert(&principal.id, req.config)
                .map_err(|err| ApiError::bad_request(err.to_string()))?;
            json!({ "ok": true, "wake": cfg })
        }
        "show" => {
            let cfg = wakes.get(&principal.id).map_err(ApiError::internal)?;
            json!({ "ok": true, "wake": cfg })
        }
        "delete" => {
            let removed = wakes.delete(&principal.id).map_err(ApiError::internal)?;
            json!({ "ok": true, "removed": removed })
        }
        "runs" => {
            let runs = wakes
                .runs(Some(&principal.id), req.limit.unwrap_or(20))
                .map_err(ApiError::internal)?;
            json!({ "ok": true, "runs": runs })
        }
        "reset" => {
            let removed = wakes
                .clear_sessions(&principal.id, None)
                .map_err(ApiError::internal)?;
            json!({ "ok": true, "removed": removed })
        }
        "test" => {
            let started = state
                .server
                .wake_account_api(
                    &principal.id,
                    req.message
                        .as_deref()
                        .map(str::trim)
                        .filter(|m| !m.is_empty())
                        .unwrap_or("manual test by the owner"),
                    req.channel.as_deref(),
                )
                .map_err(|err| ApiError::bad_request(err.to_string()))?;
            json!({ "ok": true, "started": started })
        }
        other => {
            return Err(ApiError::bad_request(format!(
                "unknown action '{other}': use set, show, delete, runs, test or reset"
            )))
        }
    };
    Ok((StatusCode::OK, with_cors_headers(), Json(body)))
}

#[derive(Deserialize)]
struct OwnerReissueReq {
    nick: String,
    invite: Option<String>,
}

/// Owner-only: revoke all of an agent's tokens and hand back a fresh one.
/// Authorised by the same owner key that guards registration.
async fn owner_reissue_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<OwnerReissueReq>,
) -> Result<impl IntoResponse, ApiError> {
    let Some(expected) = state.info.register_key.as_deref() else {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "token reissue needs an owner key to be configured: start aircd without --open-registration",
        ));
    };
    let supplied = headers
        .get("x-aircd-invite")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .or_else(|| req.invite.clone())
        .unwrap_or_default();
    if !constant_time_eq(supplied.trim().as_bytes(), expected.as_bytes()) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "token reissue requires the owner's key: send it as \"invite\" in the JSON body or in the X-AirCD-Invite header",
        ));
    }

    let auth = state.server.auth();
    let principal = auth
        .resolve_by_name(&req.nick)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(format!("no agent named '{}'", req.nick.trim())))?;
    let token = auth
        .reissue_all(&principal.id)
        .map_err(ApiError::internal)?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({ "ok": true, "principal": principal, "token": token })),
    ))
}

#[derive(Deserialize, Default)]
struct LoginReq {
    channels: Option<Vec<String>>,
    rotate_token: Option<bool>,
    label: Option<String>,
}

async fn login_agent(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<LoginReq>>,
) -> Result<impl IntoResponse, ApiError> {
    let principal = authenticate_request(&state, &headers)?;
    let req = body.map(|b| b.0).unwrap_or_default();
    let channels = req.channels.unwrap_or_else(|| vec!["#general".to_string()]);
    let joined = state
        .server
        .join_channels(&principal, &channels)
        .map_err(ApiError::internal)?;
    let rotated = if req.rotate_token.unwrap_or(false) {
        Some(
            state
                .server
                .auth()
                .issue_token(&principal.id, req.label.as_deref(), false)
                .map_err(ApiError::internal)?,
        )
    } else {
        None
    };
    let webhook = state
        .server
        .webhooks()
        .get(&principal.id)
        .map_err(ApiError::internal)?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({
            "ok": true,
            "principal": principal,
            "channels": joined,
            "webhook": webhook,
            "rotated_token": rotated,
        })),
    ))
}

async fn get_me(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    let principal = authenticate_request(&state, &headers)?;
    let webhook = state
        .server
        .webhooks()
        .get(&principal.id)
        .map_err(ApiError::internal)?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({
            "ok": true,
            "principal": principal,
            "webhook": webhook,
        })),
    ))
}

#[derive(Deserialize)]
struct JoinChannelsReq {
    channels: Option<Vec<String>>,
    channel: Option<String>,
}

async fn join_channels(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<JoinChannelsReq>,
) -> Result<impl IntoResponse, ApiError> {
    let principal = authenticate_request(&state, &headers)?;
    let mut list = req.channels.unwrap_or_default();
    if let Some(ch) = req.channel {
        list.push(ch);
    }
    if list.is_empty() {
        return Err(ApiError::bad_request("channels or channel is required"));
    }
    let joined = state
        .server
        .join_channels(&principal, &list)
        .map_err(ApiError::internal)?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({ "ok": true, "channels": joined })),
    ))
}

#[derive(Deserialize)]
struct MessagesQuery {
    channel: Option<String>,
    dm: Option<String>,
    peer: Option<String>,
    after_seq: Option<i64>,
    limit: Option<i64>,
}

async fn list_messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<MessagesQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let principal = authenticate_request(&state, &headers)?;
    let after_seq = q.after_seq.unwrap_or(0);
    let limit = q.limit.unwrap_or(100);

    let messages = if let Some(dm_target) = q.dm.as_deref().or(q.peer.as_deref()) {
        let peer_opt = if dm_target.eq_ignore_ascii_case("all") || dm_target.trim().is_empty() {
            None
        } else {
            let resolved = state
                .server
                .auth()
                .resolve_by_name(dm_target)
                .map_err(ApiError::internal)?
                .ok_or_else(|| ApiError::not_found(format!("Unknown agent '{dm_target}'")))?;
            Some(resolved.id)
        };
        state
            .server
            .history()
            .list_dm_messages(&principal.id, peer_opt.as_deref(), after_seq, limit)
            .map_err(ApiError::internal)?
    } else {
        let chan = normalize_http_channel(q.channel.as_deref().unwrap_or("#general"));
        state
            .server
            .history()
            .tail(&chan, after_seq, limit)
            .map_err(ApiError::internal)?
    };

    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({ "ok": true, "messages": messages })),
    ))
}

#[derive(Deserialize)]
struct PostMessageReq {
    target: Option<String>,
    channel: Option<String>,
    to: Option<String>,
    body: String,
}

async fn post_message(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<PostMessageReq>,
) -> Result<impl IntoResponse, ApiError> {
    let principal = authenticate_request(&state, &headers)?;
    let target = req
        .target
        .or(req.to)
        .or(req.channel)
        .unwrap_or_else(|| "#general".to_string());

    let message = state
        .server
        .post_message(&principal, &target, &req.body)
        .map_err(|err| ApiError::bad_request(err.to_string()))?;

    Ok((
        StatusCode::CREATED,
        with_cors_headers(),
        Json(json!({ "ok": true, "message": message })),
    ))
}

async fn list_roles(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    let _ = authenticate_request(&state, &headers)?;
    let snapshot = state
        .server
        .directory_snapshot()
        .map_err(ApiError::internal)?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({ "ok": true, "agents": snapshot.agents })),
    ))
}

#[derive(Deserialize)]
struct SetRoleReq {
    nick: String,
    role: String,
    channel: Option<String>,
}

async fn set_role(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<SetRoleReq>,
) -> Result<impl IntoResponse, ApiError> {
    let actor = authenticate_request(&state, &headers)?;
    let updated = state
        .server
        .set_role_api(&actor, &req.nick, &req.role, req.channel.as_deref())
        .map_err(|err| ApiError::bad_request(err.to_string()))?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({ "ok": true, "principal": updated })),
    ))
}

async fn get_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    let principal = authenticate_request(&state, &headers)?;
    let cfg = state
        .server
        .webhooks()
        .get(&principal.id)
        .map_err(ApiError::internal)?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({ "ok": true, "webhook": cfg })),
    ))
}

#[derive(Deserialize)]
struct SetWebhookReq {
    url: String,
    events: Option<Vec<String>>,
    notify_when: Option<String>,
    rotate_secret: Option<bool>,
}

async fn set_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<SetWebhookReq>,
) -> Result<impl IntoResponse, ApiError> {
    let principal = authenticate_request(&state, &headers)?;
    let cfg = state
        .server
        .webhooks()
        .upsert(
            &principal.id,
            &req.url,
            req.events,
            req.notify_when.as_deref(),
            req.rotate_secret.unwrap_or(false),
        )
        .map_err(|err| ApiError::bad_request(err.to_string()))?;

    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({
            "ok": true,
            "webhook": cfg,
            "secret": cfg.secret,
        })),
    ))
}

async fn delete_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    let principal = authenticate_request(&state, &headers)?;
    let deleted = state
        .server
        .webhooks()
        .delete(&principal.id)
        .map_err(ApiError::internal)?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({ "ok": true, "deleted": deleted })),
    ))
}

async fn test_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    let principal = authenticate_request(&state, &headers)?;
    let delivery = state
        .server
        .webhooks()
        .send_test_ping(&principal.id, &principal.nick)
        .await
        .map_err(|err| ApiError::new(StatusCode::BAD_GATEWAY, err.to_string()))?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({ "ok": true, "delivery": delivery })),
    ))
}

#[derive(Deserialize)]
struct LimitQuery {
    limit: Option<i64>,
}

async fn list_webhook_deliveries(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<LimitQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let principal = authenticate_request(&state, &headers)?;
    let deliveries = state
        .server
        .webhooks()
        .list_deliveries(&principal.id, q.limit.unwrap_or(30))
        .map_err(ApiError::internal)?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({ "ok": true, "deliveries": deliveries })),
    ))
}

#[derive(Deserialize)]
struct CreateReportReq {
    channel: Option<String>,
    title: String,
    status: Option<String>,
    summary: Option<String>,
    body: Option<String>,
    task_id: Option<String>,
    git_repo: Option<String>,
    git_branch: Option<String>,
    git_commit: Option<String>,
}

async fn create_report(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CreateReportReq>,
) -> Result<impl IntoResponse, ApiError> {
    let principal = authenticate_request(&state, &headers)?;
    let chan = normalize_http_channel(req.channel.as_deref().unwrap_or("#general"));
    let report = state
        .server
        .post_report(
            &principal,
            &chan,
            &req.title,
            req.status.as_deref().unwrap_or("info"),
            req.summary.as_deref(),
            req.body.as_deref().unwrap_or(""),
            req.task_id.as_deref(),
            req.git_repo.as_deref(),
            req.git_branch.as_deref(),
            req.git_commit.as_deref(),
        )
        .map_err(|err| ApiError::bad_request(err.to_string()))?;

    Ok((
        StatusCode::CREATED,
        with_cors_headers(),
        Json(json!({ "ok": true, "report": report })),
    ))
}

#[derive(Deserialize)]
struct ReportsQuery {
    channel: Option<String>,
    author: Option<String>,
    limit: Option<i64>,
}

async fn list_reports(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ReportsQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let _ = authenticate_request(&state, &headers)?;
    let channel = q.channel.as_deref().map(normalize_http_channel);
    let author_id = q
        .author
        .map(|a| a.trim().trim_start_matches('@').to_ascii_lowercase());
    let reports = state
        .server
        .reports()
        .list(
            channel.as_deref(),
            author_id.as_deref(),
            q.limit.unwrap_or(50),
        )
        .map_err(ApiError::internal)?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({ "ok": true, "reports": reports })),
    ))
}

async fn get_report(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let _ = authenticate_request(&state, &headers)?;
    let report = state
        .server
        .reports()
        .get(&id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(format!("Report '{id}' not found")))?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({ "ok": true, "report": report })),
    ))
}

#[derive(Deserialize)]
struct ChannelQuery {
    channel: Option<String>,
}

async fn list_tasks(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ChannelQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let _ = authenticate_request(&state, &headers)?;
    let channel = normalize_http_channel(q.channel.as_deref().unwrap_or("#general"));
    let tasks = state
        .server
        .tasks()
        .list(&channel)
        .map_err(ApiError::internal)?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({ "ok": true, "channel": channel, "tasks": tasks })),
    ))
}

#[derive(Deserialize)]
struct CreateTaskReq {
    channel: Option<String>,
    title: String,
    body: Option<String>,
    kind: Option<String>,
    assignee: Option<String>,
    repo: Option<String>,
    branch: Option<String>,
    issue_url: Option<String>,
}

async fn create_task(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CreateTaskReq>,
) -> Result<impl IntoResponse, ApiError> {
    let actor = authenticate_request(&state, &headers)?;
    let raw_channel = req.channel.as_deref().ok_or_else(|| {
        ApiError::bad_request("channel is required: name the project channel this task belongs to")
    })?;
    let channel = normalize_http_channel(raw_channel);
    let task = state
        .server
        .create_task_full_api(
            &actor,
            crate::tasks::NewTask {
                channel,
                title: req.title,
                body: req.body,
                kind: req.kind,
                assignee: req.assignee,
                created_by: None,
                repo: req.repo,
                branch: req.branch,
                issue_url: req.issue_url,
            },
        )
        .map_err(|err| ApiError::bad_request(err.to_string()))?;
    Ok((
        StatusCode::CREATED,
        with_cors_headers(),
        Json(json!({ "ok": true, "task": task })),
    ))
}

#[derive(Deserialize)]
struct AssignTaskReq {
    assignee: Option<String>,
}

async fn assign_task(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<AssignTaskReq>,
) -> Result<impl IntoResponse, ApiError> {
    let actor = authenticate_request(&state, &headers)?;
    let task = state
        .server
        .assign_task_api(&actor, &id, req.assignee.as_deref())
        .map_err(|err| ApiError::bad_request(err.to_string()))?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({ "ok": true, "task": task })),
    ))
}

#[derive(Deserialize)]
struct TaskStatusReq {
    status: String,
    note: Option<String>,
    branch: Option<String>,
}

async fn set_task_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<TaskStatusReq>,
) -> Result<impl IntoResponse, ApiError> {
    let actor = authenticate_request(&state, &headers)?;
    let task = state
        .server
        .set_task_status_api(
            &actor,
            &id,
            &req.status,
            req.note.as_deref(),
            req.branch.as_deref(),
        )
        .map_err(|err| ApiError::bad_request(err.to_string()))?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({ "ok": true, "task": task })),
    ))
}

async fn get_task(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let _ = authenticate_request(&state, &headers)?;
    let task = state
        .server
        .get_task_api(&id)
        .map_err(|err| ApiError::not_found(err.to_string()))?;
    let events = state
        .server
        .tasks()
        .events(&task.id)
        .map_err(ApiError::internal)?;
    let question = state
        .server
        .tasks()
        .pending_question(&task.id)
        .map_err(ApiError::internal)?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({ "ok": true, "task": task, "events": events, "question": question })),
    ))
}

async fn get_inbox(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    let actor = authenticate_request(&state, &headers)?;
    let inbox = state
        .server
        .inbox_api(&actor)
        .map_err(ApiError::internal)?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({ "ok": true, "inbox": inbox })),
    ))
}

#[derive(Deserialize)]
struct CreateProjectReq {
    channel: String,
    name: Option<String>,
    description: Option<String>,
}

async fn create_project(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CreateProjectReq>,
) -> Result<impl IntoResponse, ApiError> {
    let actor = authenticate_request(&state, &headers)?;
    let channel = normalize_http_channel(&req.channel);
    let name = req
        .name
        .clone()
        .unwrap_or_else(|| channel.trim_start_matches('#').to_string());
    let project = state
        .server
        .create_project_api(&actor, &channel, &name, req.description.as_deref())
        .map_err(|err| ApiError::bad_request(err.to_string()))?;
    Ok((
        StatusCode::CREATED,
        with_cors_headers(),
        Json(json!({ "ok": true, "project": project })),
    ))
}

async fn list_projects(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    let _ = authenticate_request(&state, &headers)?;
    let projects = state
        .server
        .projects()
        .list()
        .map_err(ApiError::internal)?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({ "ok": true, "projects": projects })),
    ))
}

#[derive(Deserialize)]
struct ProjectMemberReq {
    channel: String,
    nick: String,
    role: Option<String>,
}

async fn set_project_member(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ProjectMemberReq>,
) -> Result<impl IntoResponse, ApiError> {
    let actor = authenticate_request(&state, &headers)?;
    let channel = normalize_http_channel(&req.channel);
    let member = state
        .server
        .set_project_member_api(&actor, &channel, &req.nick, req.role.as_deref())
        .map_err(|err| ApiError::bad_request(err.to_string()))?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({ "ok": true, "channel": channel, "member": member })),
    ))
}

async fn list_project_members(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ChannelQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let _ = authenticate_request(&state, &headers)?;
    let raw = q
        .channel
        .ok_or_else(|| ApiError::bad_request("channel is required"))?;
    let channel = normalize_http_channel(&raw);
    let members = state
        .server
        .projects()
        .members(&channel)
        .map_err(ApiError::internal)?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({ "ok": true, "channel": channel, "members": members })),
    ))
}

#[derive(Deserialize)]
struct TaskTextReq {
    question: Option<String>,
    answer: Option<String>,
    note: Option<String>,
}

async fn ask_task(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<TaskTextReq>,
) -> Result<impl IntoResponse, ApiError> {
    let actor = authenticate_request(&state, &headers)?;
    let task = state
        .server
        .ask_task_api(&actor, &id, req.question.as_deref().unwrap_or_default())
        .map_err(|err| ApiError::bad_request(err.to_string()))?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({ "ok": true, "task": task })),
    ))
}

async fn answer_task(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<TaskTextReq>,
) -> Result<impl IntoResponse, ApiError> {
    let actor = authenticate_request(&state, &headers)?;
    let task = state
        .server
        .answer_task_api(&actor, &id, req.answer.as_deref().unwrap_or_default())
        .map_err(|err| ApiError::bad_request(err.to_string()))?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({ "ok": true, "task": task })),
    ))
}

async fn escalate_task(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<TaskTextReq>,
) -> Result<impl IntoResponse, ApiError> {
    let actor = authenticate_request(&state, &headers)?;
    let task = state
        .server
        .escalate_task_api(&actor, &id, req.note.as_deref())
        .map_err(|err| ApiError::bad_request(err.to_string()))?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({ "ok": true, "task": task })),
    ))
}

#[derive(Deserialize)]
struct NotesQuery {
    nick: Option<String>,
}

/// An account reads its own notes; the owner may read anyone's.
async fn get_notes(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<NotesQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let actor = authenticate_request(&state, &headers)?;
    let target_id = match q.nick.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        Some(nick) if !nick.trim_start_matches('@').eq_ignore_ascii_case(&actor.id) => {
            if !Server::is_owner(&actor) {
                return Err(ApiError::new(
                    StatusCode::FORBIDDEN,
                    "only the owner can read another account's notes",
                ));
            }
            state
                .server
                .auth()
                .resolve_by_name(nick)
                .map_err(ApiError::internal)?
                .ok_or_else(|| ApiError::not_found(format!("no agent named '{nick}'")))?
                .id
        }
        _ => actor.id.clone(),
    };
    let notes = state
        .server
        .notes()
        .get(&target_id)
        .map_err(ApiError::internal)?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({ "ok": true, "notes": notes })),
    ))
}

#[derive(Deserialize)]
struct WriteNotesReq {
    body: String,
    append: Option<bool>,
}

async fn write_notes(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<WriteNotesReq>,
) -> Result<impl IntoResponse, ApiError> {
    let actor = authenticate_request(&state, &headers)?;
    let notes = state
        .server
        .notes()
        .write(&actor.id, &req.body, req.append.unwrap_or(false))
        .map_err(|err| ApiError::bad_request(err.to_string()))?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({ "ok": true, "notes": notes })),
    ))
}

#[derive(Deserialize)]
struct ProjectPmReq {
    channel: String,
    nick: String,
}

async fn set_project_pm(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ProjectPmReq>,
) -> Result<impl IntoResponse, ApiError> {
    let actor = authenticate_request(&state, &headers)?;
    let channel = normalize_http_channel(&req.channel);
    let project = state
        .server
        .set_project_pm_api(&actor, &channel, &req.nick)
        .map_err(|err| ApiError::bad_request(err.to_string()))?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({ "ok": true, "project": project })),
    ))
}

async fn claim_task(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let actor = authenticate_request(&state, &headers)?;
    let task = state
        .server
        .claim_task_api(&actor, &id)
        .map_err(|err| ApiError::conflict(err.to_string()))?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({ "ok": true, "task": task })),
    ))
}

async fn done_task(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let actor = authenticate_request(&state, &headers)?;
    let task = state
        .server
        .done_task_api(&actor, &id)
        .map_err(|err| ApiError::conflict(err.to_string()))?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({ "ok": true, "task": task })),
    ))
}

async fn release_task(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let actor = authenticate_request(&state, &headers)?;
    let task = state
        .server
        .release_task_api(&actor, &id)
        .map_err(|err| ApiError::conflict(err.to_string()))?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({ "ok": true, "task": task })),
    ))
}

#[derive(Deserialize)]
struct AcquireLockReq {
    resource: String,
    channel: Option<String>,
    ttl_seconds: Option<i64>,
    note: Option<String>,
}

async fn acquire_lock(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<AcquireLockReq>,
) -> Result<impl IntoResponse, ApiError> {
    let actor = authenticate_request(&state, &headers)?;
    let chan = normalize_http_channel(req.channel.as_deref().unwrap_or("#general"));
    let lock = state
        .server
        .acquire_lock_api(
            &actor,
            &req.resource,
            &chan,
            req.note.as_deref(),
            req.ttl_seconds,
        )
        .map_err(|err| {
            if err.downcast_ref::<LockConflictError>().is_some() {
                ApiError::conflict(err.to_string())
            } else {
                ApiError::bad_request(err.to_string())
            }
        })?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({ "ok": true, "lock": lock })),
    ))
}

#[derive(Deserialize)]
struct ReleaseLockReq {
    resource: String,
    force: Option<bool>,
}

async fn release_lock_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ReleaseLockReq>,
) -> Result<impl IntoResponse, ApiError> {
    let actor = authenticate_request(&state, &headers)?;
    let is_privileged = matches!(
        actor.role.as_str(),
        "leader" | "coordinator" | "op" | "admin"
    );
    let force = req.force.unwrap_or(false) && is_privileged;
    if req.force.unwrap_or(false) && !is_privileged {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "Only leader/coordinator roles can force-unlock another agent's resource",
        ));
    }
    let released = state
        .server
        .release_lock_api(&actor, &req.resource, force)
        .map_err(|err| {
            if err.downcast_ref::<LockConflictError>().is_some() {
                ApiError::conflict(err.to_string())
            } else {
                ApiError::bad_request(err.to_string())
            }
        })?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({ "ok": true, "released": released })),
    ))
}

async fn release_lock_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(resource): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let actor = authenticate_request(&state, &headers)?;
    let is_privileged = matches!(
        actor.role.as_str(),
        "leader" | "coordinator" | "op" | "admin"
    );
    let released = state
        .server
        .release_lock_api(&actor, &resource, is_privileged)
        .map_err(|err| {
            if err.downcast_ref::<LockConflictError>().is_some() {
                ApiError::conflict(err.to_string())
            } else {
                ApiError::bad_request(err.to_string())
            }
        })?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({ "ok": true, "released": released })),
    ))
}

#[derive(Deserialize)]
struct RegisterGitRepoReq {
    name: String,
    channel: Option<String>,
    local_path: String,
    remote_url: Option<String>,
    default_branch: Option<String>,
    webhook_secret: Option<String>,
}

async fn register_git_repo(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<RegisterGitRepoReq>,
) -> Result<impl IntoResponse, ApiError> {
    let actor = authenticate_request(&state, &headers)?;
    let clean_name =
        sanitize_repo_name(&req.name).map_err(|err| ApiError::bad_request(err.to_string()))?;
    let channel = normalize_http_channel(req.channel.as_deref().unwrap_or_else(|| &clean_name));
    let (repo, newly_issued_secret) = state
        .server
        .gits()
        .upsert(
            &clean_name,
            &channel,
            &req.local_path,
            req.remote_url.as_deref(),
            req.default_branch.as_deref(),
            req.webhook_secret.as_deref(),
        )
        .map_err(|err| ApiError::bad_request(err.to_string()))?;

    let _ = state
        .server
        .join_channels(&actor, std::slice::from_ref(&channel));

    Ok((
        StatusCode::CREATED,
        with_cors_headers(),
        Json(json!({
            "ok": true,
            "git_repo": repo,
            "webhook_secret": newly_issued_secret,
        })),
    ))
}

#[derive(Deserialize)]
struct PostGitEventReq {
    repo: String,
    channel: Option<String>,
    event: Option<String>,
    branch: Option<String>,
    commit: Option<String>,
    title: Option<String>,
    url: Option<String>,
    summary: String,
}

async fn post_git_event(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<PostGitEventReq>,
) -> Result<impl IntoResponse, ApiError> {
    let actor = authenticate_request(&state, &headers)?;
    let (message, channel) = state
        .server
        .post_git_event(
            &actor.id,
            &actor.nick,
            &req.repo,
            req.channel.as_deref(),
            req.event.as_deref().unwrap_or("commit"),
            req.branch.as_deref(),
            req.commit.as_deref(),
            req.title.as_deref(),
            req.url.as_deref(),
            req.summary.trim(),
        )
        .map_err(|err| ApiError::bad_request(err.to_string()))?;

    Ok((
        StatusCode::CREATED,
        with_cors_headers(),
        Json(json!({
            "ok": true,
            "channel": channel,
            "message": message,
        })),
    ))
}

#[derive(Deserialize)]
struct GitHubWebhookQuery {
    repo: Option<String>,
}

async fn github_webhook_query(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<GitHubWebhookQuery>,
    body: Bytes,
) -> Result<impl IntoResponse, ApiError> {
    handle_github_webhook(&state, &headers, q.repo.as_deref(), &body)
}

async fn github_webhook_repo(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(repo): Path<String>,
    body: Bytes,
) -> Result<impl IntoResponse, ApiError> {
    handle_github_webhook(&state, &headers, Some(&repo), &body)
}

fn handle_github_webhook(
    state: &AppState,
    headers: &HeaderMap,
    repo_hint: Option<&str>,
    raw_body: &[u8],
) -> Result<Response, ApiError> {
    let payload: Value = serde_json::from_slice(raw_body)
        .map_err(|err| ApiError::bad_request(format!("Invalid GitHub JSON payload: {err}")))?;

    let derived_repo = repo_hint
        .map(str::to_string)
        .or_else(|| {
            payload
                .get("repository")
                .and_then(|r| r.get("name"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .ok_or_else(|| ApiError::bad_request("Missing repository name"))?;

    let clean_repo =
        sanitize_repo_name(&derived_repo).map_err(|err| ApiError::bad_request(err.to_string()))?;
    let Some((repo_row, maybe_secret)) = state
        .server
        .gits()
        .get_with_secret(&clean_repo)
        .map_err(ApiError::internal)?
    else {
        return Err(ApiError::not_found(format!(
            "Git repo '{clean_repo}' is not registered in aircd"
        )));
    };

    let secret = maybe_secret
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ApiError::unauthorized("Repo has no webhook_secret configured"))?;
    let sig_header = headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if !verify_github_signature(secret, raw_body, sig_header) {
        return Err(ApiError::unauthorized("Invalid X-Hub-Signature-256"));
    }

    let gh_event = headers
        .get("x-github-event")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("push");
    let sender = payload
        .get("sender")
        .and_then(|s| s.get("login"))
        .and_then(Value::as_str)
        .unwrap_or("github");

    let (branch, commit, title, url, summary) = extract_github_summary(gh_event, &payload);
    let (message, channel) = state
        .server
        .post_git_event(
            "github",
            sender,
            &repo_row.name,
            Some(&repo_row.channel),
            gh_event,
            branch.as_deref(),
            commit.as_deref(),
            title.as_deref(),
            url.as_deref(),
            &summary,
        )
        .map_err(ApiError::internal)?;

    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({
            "ok": true,
            "channel": channel,
            "message": message,
        })),
    )
        .into_response())
}

fn extract_github_summary(
    event: &str,
    payload: &Value,
) -> (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    String,
) {
    match event {
        "pull_request" => {
            let action = payload
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or("updated");
            let pr = payload.get("pull_request").unwrap_or(&Value::Null);
            let num = pr.get("number").and_then(Value::as_i64).unwrap_or(0);
            let title = pr.get("title").and_then(Value::as_str).map(str::to_string);
            let branch = pr
                .get("head")
                .and_then(|h| h.get("ref"))
                .and_then(Value::as_str)
                .map(str::to_string);
            let sha = pr
                .get("head")
                .and_then(|h| h.get("sha"))
                .and_then(Value::as_str)
                .map(|s| s.chars().take(12).collect());
            let url = pr
                .get("html_url")
                .and_then(Value::as_str)
                .map(str::to_string);
            let summary = format!(
                "PR #{num} {action}: {}",
                title.as_deref().unwrap_or("(untitled)")
            );
            (branch, sha, title, url, summary)
        }
        "push" => {
            let git_ref = payload
                .get("ref")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let branch = git_ref
                .strip_prefix("refs/heads/")
                .unwrap_or(git_ref)
                .to_string();
            let head_commit = payload.get("head_commit").unwrap_or(&Value::Null);
            let sha = head_commit
                .get("id")
                .or_else(|| payload.get("after"))
                .and_then(Value::as_str)
                .map(|s| s.chars().take(12).collect());
            let msg = head_commit
                .get("message")
                .and_then(Value::as_str)
                .and_then(|m| m.lines().next())
                .unwrap_or("push event")
                .to_string();
            let url = payload
                .get("compare")
                .and_then(Value::as_str)
                .map(str::to_string);
            let count = payload
                .get("commits")
                .and_then(Value::as_array)
                .map(|a| a.len())
                .unwrap_or(1);
            let summary = format!("{count} commit(s): {msg}");
            (
                (!branch.is_empty()).then_some(branch),
                sha,
                Some(msg),
                url,
                summary,
            )
        }
        _ => {
            let action = payload
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or("event");
            (None, None, None, None, format!("{event} ({action})"))
        }
    }
}

fn authenticate_request(state: &AppState, headers: &HeaderMap) -> Result<Principal, ApiError> {
    let raw = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|auth| {
            auth.strip_prefix("Bearer ")
                .or_else(|| auth.strip_prefix("bearer "))
        })
        .or_else(|| {
            headers
                .get("x-aircd-token")
                .and_then(|v| v.to_str().ok())
        })
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            ApiError::unauthorized(
                "Missing Authorization: Bearer <token> header; register via POST /v1/register first",
            )
        })?;

    state
        .server
        .auth()
        .authenticate(raw)
        .map_err(|_| ApiError::unauthorized("Invalid bearer token"))
}

fn build_welcome_payload(state: &AppState, base_host: &str) -> Result<Value, ApiError> {
    let snapshot = state
        .server
        .directory_snapshot()
        .map_err(ApiError::internal)?;
    let base_http = format!("http://{}:{}", base_host, state.info.http_port);
    Ok(json!({
        "ok": true,
        "service": "aircd",
        "purpose": "Lightweight CLI-driven IRC + Webhook + Roles + Equipment Locks + Git coordination server",
        "connection": {
            "http_url": base_http,
            "irc_host": base_host,
            "irc_port": state.info.irc_port,
            "tls_port": state.info.tls_port,
            "detected_hosts": state.info.detected_hosts,
            "reachable_welcome_urls": state.info.detected_hosts.iter().map(|h| format!("http://{}:{}", h, state.info.http_port)).collect::<Vec<_>>(),
        },
        "onboarding": {
            "step_1_register": format!("curl -sX POST {base_http}/v1/register -H \"Content-Type: application/json\" -d '{{\"nick\":\"codex\",\"role\":\"leader\",\"channels\":[\"#general\"]}}'"),
            "step_2_set_webhook": format!("curl -sX POST {base_http}/v1/webhooks -H \"Authorization: Bearer $TOKEN\" -H \"Content-Type: application/json\" -d '{{\"url\":\"http://127.0.0.1:8787/hook\",\"events\":[\"message\",\"mention\",\"dm\",\"report\",\"task\",\"git\",\"role\",\"lock\"],\"notify_when\":\"mentions\"}}'"),
            "step_3_test_webhook": format!("curl -sX POST {base_http}/v1/webhooks/test -H \"Authorization: Bearer $TOKEN\""),
            "step_4_send_message": format!("curl -sX POST {base_http}/v1/messages -H \"Authorization: Bearer $TOKEN\" -H \"Content-Type: application/json\" -d '{{\"target\":\"#general\",\"body\":\"@claude ready for PR review\"}}'"),
            "step_5_submit_report": format!("curl -sX POST {base_http}/v1/reports -H \"Authorization: Bearer $TOKEN\" -H \"Content-Type: application/json\" -d '{{\"channel\":\"#general\",\"title\":\"Build green\",\"status\":\"completed\",\"body\":\"All tests pass.\"}}'"),
            "step_6_assign_role": format!("curl -sX POST {base_http}/v1/roles -H \"Authorization: Bearer $TOKEN\" -H \"Content-Type: application/json\" -d '{{\"nick\":\"claude\",\"role\":\"reviewer\"}}'"),
            "step_7_lock_equipment": format!("curl -sX POST {base_http}/v1/locks -H \"Authorization: Bearer $TOKEN\" -H \"Content-Type: application/json\" -d '{{\"resource\":\"quest-headset-1\",\"channel\":\"#general\",\"ttl_seconds\":1800,\"note\":\"capture test\"}}'"),
            "step_8_connect_git": format!("airc git init-workspace C:/path/to/repo --server {base_http}"),
        },
        "irc_commands": [
            "PASS <token> | NICK <nick> | USER <nick> 0 * :<desc>",
            "JOIN #channel | PART #channel | PRIVMSG <#channel|nick> :<message>",
            "NAMES [#channel] | WHO [#channel] | CHATHISTORY AFTER <#channel> <seq> <limit>",
            "ROLE <nick> <role> | ROLES",
            "LOCK <resource> [#channel] [ttl_seconds] [:note] | UNLOCK <resource> | LOCKS [#channel]",
            "TASK CREATE <#channel> :<title> | TASK CLAIM <id> | TASK DONE <id> | TASK RELEASE <id> | TASK LIST <#channel>"
        ],
        "webhook_signing": {
            "headers": ["X-AirCD-Signature-256", "X-Agent-Ping-Signature-256", "X-AirCD-Event", "X-AirCD-Delivery"],
            "algorithm": "HMAC-SHA256 hex with 'sha256=' prefix over raw JSON request body",
            "events": ["message", "mention", "dm", "report", "task", "git", "role", "lock", "connection_test"],
            "notify_when_modes": ["always", "offline", "mentions"]
        },
        "directory": snapshot,
    }))
}

fn build_welcome_markdown(state: &AppState, base_host: &str) -> Result<String, ApiError> {
    let base_http = format!("http://{}:{}", base_host, state.info.http_port);
    let snapshot = state
        .server
        .directory_snapshot()
        .map_err(ApiError::internal)?;
    Ok(format!(
        "# aircd — IRC + Webhook + Git Coordinator\n\n\
         - **HTTP Welcome / API:** `{base_http}`\n\
         - **IRC TCP:** `{base_host}:{}`\n\
         - **Active Agents:** {} | **Channels:** {} | **Git Repos:** {} | **Locks:** {}\n\n\
         ## 1. Register & Save Token\n```sh\n\
         airc register --server {base_http} --nick codex --role leader --channels #general\n\
         ```\n\n\
         ## 2. Configure Signed Webhook & Listener\n```sh\n\
         airc webhook listen --port 8787 --register\n\
         airc webhook test\n\
         ```\n\n\
         ## 3. Send Messages, DMs, Reports, Roles & Equipment Locks\n```sh\n\
         airc send #general \"@claude check PR #4\"\n\
         airc send @claude \"DM: headset is free\"\n\
         airc role claude reviewer\n\
         airc lock quest-headset-1 --ttl 1800 --note \"capture run\"\n\
         airc report create --channel #general --title \"PR Ready\" --status completed --body \"Tests pass\"\n\
         ```\n\n\
         ## 4. Connect Git Repo & Install Hooks\n```sh\n\
         airc git init-workspace C:/path/to/repo --server {base_http}\n\
         airc git pr-report C:/path/to/repo --base main --status completed\n\
         ```\n",
        state.info.irc_port,
        snapshot.agents.len(),
        snapshot.channels.len(),
        snapshot.git_repos.len(),
        snapshot.locks.len(),
    ))
}

fn build_welcome_html(state: &AppState, base_host: &str) -> Result<String, ApiError> {
    let snapshot = state
        .server
        .directory_snapshot()
        .map_err(ApiError::internal)?;
    let base_http = format!("http://{}:{}", base_host, state.info.http_port);
    let irc_endpoint = format!("{base_host}:{}", state.info.irc_port);

    let agent_rows = if snapshot.agents.is_empty() {
        "<tr><td colspan=\"5\">No agents yet</td></tr>".to_string()
    } else {
        snapshot
            .agents
            .iter()
            .map(|a| {
                let status_badge = if a.online { "online" } else { "offline" };
                let hook_badge = if a.webhook_configured { "configured" } else { "none" };
                format!(
                    "<tr><td>@{}</td><td><code>{}</code></td><td>{}</td><td>{}</td><td>{}</td></tr>",
                    escape_html(&a.nick),
                    escape_html(&a.role),
                    escape_html(a.team.as_deref().unwrap_or("-")),
                    status_badge,
                    hook_badge,
                )
            })
            .collect::<Vec<_>>()
            .join("")
    };

    let channel_rows = if snapshot.channels.is_empty() {
        "<tr><td colspan=\"2\">No channel history yet (send a message to #general)</td></tr>"
            .to_string()
    } else {
        snapshot
            .channels
            .iter()
            .map(|c| {
                format!(
                    "<tr><td><code>{}</code></td><td>{}</td></tr>",
                    escape_html(&c.channel),
                    c.message_count
                )
            })
            .collect::<Vec<_>>()
            .join("")
    };

    let git_rows = if snapshot.git_repos.is_empty() {
        "<tr><td colspan=\"4\">No connected git repos yet</td></tr>".to_string()
    } else {
        snapshot
            .git_repos
            .iter()
            .map(|g| {
                format!(
                    "<tr><td><code>{}</code></td><td><code>{}</code></td><td>{}</td><td>{}</td></tr>",
                    escape_html(&g.name),
                    escape_html(&g.channel),
                    escape_html(g.last_branch.as_deref().unwrap_or(g.default_branch.as_deref().unwrap_or("-"))),
                    escape_html(&g.local_path),
                )
            })
            .collect::<Vec<_>>()
            .join("")
    };

    let lock_rows = if snapshot.locks.is_empty() {
        "<tr><td colspan=\"4\">No active equipment or branch locks</td></tr>".to_string()
    } else {
        snapshot
            .locks
            .iter()
            .map(|l| {
                format!(
                    "<tr><td><code>{}</code></td><td>@{}</td><td><code>{}</code></td><td>{}</td></tr>",
                    escape_html(&l.resource),
                    escape_html(&l.owner_nick),
                    escape_html(&l.channel),
                    escape_html(l.note.as_deref().unwrap_or("-")),
                )
            })
            .collect::<Vec<_>>()
            .join("")
    };

    let reachable_urls = state
        .info
        .detected_hosts
        .iter()
        .map(|h| {
            format!(
                "<li><code>http://{}:{}</code> &nbsp;|&nbsp; IRC <code>{}:{}</code></li>",
                escape_html(h),
                state.info.http_port,
                escape_html(h),
                state.info.irc_port
            )
        })
        .collect::<Vec<_>>()
        .join("");

    Ok(format!(
        r####"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>aircd — IRC + Webhook + Git Coordinator</title>
<style>
  :root {{ color-scheme: dark; --bg: #0f1419; --card: #171f29; --line: #283546; --fg: #e7eef6; --muted: #91a4b8; --accent: #58b4ff; --ok: #4dd48d; }}
  body {{ margin: 0; padding: 24px; font: 14px/1.5 system-ui, -apple-system, sans-serif; background: var(--bg); color: var(--fg); }}
  .wrap {{ max-width: 1080px; margin: 0 auto; display: grid; gap: 18px; }}
  .card {{ background: var(--card); border: 1px solid var(--line); border-radius: 10px; padding: 16px 20px; }}
  .grid {{ display: grid; gap: 16px; grid-template-columns: repeat(auto-fit, minmax(300px, 1fr)); }}
  h1, h2, h3 {{ margin: 0 0 10px; font-weight: 600; }}
  h1 {{ font-size: 22px; color: var(--accent); }}
  h2 {{ font-size: 16px; }}
  pre, code {{ font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; background: #0b0f14; border-radius: 6px; }}
  pre {{ padding: 12px; overflow-x: auto; border: 1px solid var(--line); margin: 8px 0; }}
  code {{ padding: 2px 5px; }}
  table {{ width: 100%; border-collapse: collapse; }}
  th, td {{ text-align: left; padding: 7px 8px; border-bottom: 1px solid var(--line); font-size: 13px; }}
  th {{ color: var(--muted); font-weight: 500; }}
  input, select, textarea, button {{ font: inherit; color: var(--fg); background: #0b0f14; border: 1px solid var(--line); border-radius: 6px; padding: 7px 10px; }}
  button {{ background: #1c5a99; border-color: #2f75bd; cursor: pointer; font-weight: 600; }}
  button:hover {{ background: #236cb8; }}
  .row {{ display: flex; flex-wrap: wrap; gap: 8px; margin-bottom: 8px; }}
  .muted {{ color: var(--muted); }}
</style>
</head>
<body>
<div class="wrap">
  <div class="card">
    <h1>aircd — IRC + Webhook + Roles + Git Coordinator</h1>
    <p class="muted">Lightweight agent &amp; developer coordination bus. Connect via raw IRC (<code>{irc}</code>), the <code>airc</code> CLI, or HTTP REST API (<code>{http}</code>).</p>
    <ul>{reachable_urls}</ul>
    <p class="muted">Machine-readable specs: <a style="color:var(--accent)" href="/v1/welcome">/v1/welcome</a> (JSON) &middot; <a style="color:var(--accent)" href="/?format=md">/?format=md</a> (Markdown) &middot; <a style="color:var(--accent)" href="/healthz">/healthz</a></p>
  </div>

  <div class="grid">
    <div class="card">
      <h2>1. Quick CLI Onboarding</h2>
<pre>airc register --server {http} --nick codex --role leader
airc webhook listen --port 8787 --register
airc webhook test
airc send #general "@claude ready to pair on PRs"
airc role claude reviewer
airc lock quest-headset-1 --ttl 1800 --note "capture"
airc report create --channel #general --title "Status" --status completed --body "All good"
airc git init-workspace C:/path/to/repo --server {http}</pre>
    </div>

    <div class="card">
      <h2>2. HTTP / curl Onboarding</h2>
<pre># Register and capture Bearer token (saved once)
curl -sX POST {http}/v1/register -H "Content-Type: application/json" \
  -d '{{"nick":"codex","role":"leader","channels":["#general"]}}'

# Register signed webhook callback (HMAC-SHA256)
curl -sX POST {http}/v1/webhooks -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{{"url":"http://127.0.0.1:8787/hook","notify_when":"mentions"}}'</pre>
      <p class="muted">Verify inbound webhook signatures via header <code>X-AirCD-Signature-256: sha256=&lt;hmac&gt;</code> over raw JSON body.</p>
    </div>
  </div>

  <div class="grid">
    <div class="card">
      <h2>3. Interactive Register (Generates Token Once)</h2>
      <div class="row">
        <input id="regNick" placeholder="nick (e.g. codex)">
        <select id="regRole">
          <option value="leader">leader</option>
          <option value="coordinator">coordinator</option>
          <option value="reviewer">reviewer</option>
          <option value="builder">builder</option>
          <option value="tester">tester</option>
          <option value="member" selected>member</option>
        </select>
        <input id="regChannels" value="#general" placeholder="#general,#repo">
        <input id="regInvite" type="password" placeholder="owner invite key">
        <button onclick="doRegister()">Register</button>
      </div>
      <pre id="regOut">Click Register to issue a new agent token and load it into the browser console below.</pre>
    </div>

    <div class="card">
      <h2>4. Quick Message / Webhook Console</h2>
      <div class="row">
        <input id="tok" placeholder="Bearer token" style="flex:1">
      </div>
      <div class="row">
        <input id="msgTarget" value="#general" placeholder="#channel or @nick">
        <input id="msgBody" placeholder="message or @mention..." style="flex:1">
        <button onclick="doSend()">Send</button>
      </div>
      <div class="row">
        <input id="hookUrl" placeholder="http://127.0.0.1:8787/hook" style="flex:1">
        <button onclick="doSetWebhook()">Set Webhook</button>
        <button onclick="doTestWebhook()">Test Ping</button>
      </div>
      <pre id="apiOut">Ready.</pre>
    </div>
  </div>

  <div class="grid">
    <div class="card">
      <h2>Agents &amp; Roles</h2>
      <table><thead><tr><th>Nick</th><th>Role</th><th>Team</th><th>Status</th><th>Webhook</th></tr></thead><tbody>{agent_rows}</tbody></table>
    </div>
    <div class="card">
      <h2>Active Channels</h2>
      <table><thead><tr><th>Channel</th><th>Messages</th></tr></thead><tbody>{channel_rows}</tbody></table>
    </div>
  </div>

  <div class="grid">
    <div class="card">
      <h2>Dev Equipment &amp; Branch Locks</h2>
      <table><thead><tr><th>Resource</th><th>Holder</th><th>Channel</th><th>Note</th></tr></thead><tbody>{lock_rows}</tbody></table>
    </div>
    <div class="card">
      <h2>Connected Git Repositories</h2>
      <table><thead><tr><th>Repo</th><th>Channel</th><th>Branch</th><th>Path</th></tr></thead><tbody>{git_rows}</tbody></table>
    </div>
  </div>
</div>
<script>
async function doRegister() {{
  const res = await fetch('/v1/register', {{
    method: 'POST',
    headers: {{'Content-Type': 'application/json'}},
    body: JSON.stringify({{
      nick: document.getElementById('regNick').value,
      role: document.getElementById('regRole').value,
      channels: document.getElementById('regChannels').value.split(',').map(s => s.trim()).filter(Boolean),
      invite: document.getElementById('regInvite').value || undefined
    }})
  }});
  const data = await res.json();
  document.getElementById('regOut').textContent = JSON.stringify(data, null, 2);
  if (data.token) document.getElementById('tok').value = data.token;
}}
async function doSend() {{
  const res = await fetch('/v1/messages', {{
    method: 'POST',
    headers: {{'Content-Type': 'application/json', 'Authorization': 'Bearer ' + document.getElementById('tok').value}},
    body: JSON.stringify({{target: document.getElementById('msgTarget').value, body: document.getElementById('msgBody').value}})
  }});
  document.getElementById('apiOut').textContent = JSON.stringify(await res.json(), null, 2);
}}
async function doSetWebhook() {{
  const res = await fetch('/v1/webhooks', {{
    method: 'POST',
    headers: {{'Content-Type': 'application/json', 'Authorization': 'Bearer ' + document.getElementById('tok').value}},
    body: JSON.stringify({{url: document.getElementById('hookUrl').value}})
  }});
  document.getElementById('apiOut').textContent = JSON.stringify(await res.json(), null, 2);
}}
async function doTestWebhook() {{
  const res = await fetch('/v1/webhooks/test', {{
    method: 'POST',
    headers: {{'Authorization': 'Bearer ' + document.getElementById('tok').value}}
  }});
  document.getElementById('apiOut').textContent = JSON.stringify(await res.json(), null, 2);
}}
</script>
</body>
</html>"####,
        irc = escape_html(&irc_endpoint),
        http = escape_html(&base_http),
        reachable_urls = reachable_urls,
        agent_rows = agent_rows,
        channel_rows = channel_rows,
        lock_rows = lock_rows,
        git_rows = git_rows,
    ))
}

fn resolve_request_host(headers: &HeaderMap, info: &HttpRuntimeInfo) -> String {
    if let Some(raw_host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) {
        let trimmed = raw_host.trim();
        if let Some(stripped) = trimmed.strip_prefix('[') {
            if let Some((ip, _)) = stripped.split_once(']') {
                return format!("[{ip}]");
            }
        }
        if let Some((host_only, _port)) = trimmed.rsplit_once(':') {
            if !host_only.is_empty() && !host_only.contains(':') {
                return host_only.to_string();
            }
        }
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    info.detected_hosts
        .iter()
        .find(|h| *h != "127.0.0.1")
        .cloned()
        .unwrap_or_else(|| "127.0.0.1".to_string())
}

fn prefers_json(headers: &HeaderMap) -> bool {
    let accept = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    accept.contains("application/json") && !accept.contains("text/html")
}

fn normalize_http_channel(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        "#general".to_string()
    } else if trimmed.starts_with('#') || trimmed.starts_with('&') {
        trimmed.to_string()
    } else {
        format!("#{trimmed}")
    }
}

fn parse_port(bind: &str, default_port: u16) -> u16 {
    bind.rsplit_once(':')
        .and_then(|(_, p)| p.parse::<u16>().ok())
        .unwrap_or(default_port)
}

fn with_cors_headers() -> [(header::HeaderName, &'static str); 3] {
    [
        (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
        (
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            "Authorization, Content-Type, X-Aircd-Token",
        ),
        (
            header::ACCESS_CONTROL_ALLOW_METHODS,
            "GET, POST, DELETE, OPTIONS",
        ),
    ]
}

fn escape_html(raw: &str) -> String {
    raw.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store;
    use crate::webhooks::sign_payload;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn welcome_register_webhook_report_roles_locks_and_git_http_flow() -> Result<()> {
        let db = store::open(":memory:")?;
        let server = Server::new(db);
        let _worker = server.webhooks().spawn_worker();

        // Start a local test webhook receiver.
        let (hook_tx, mut hook_rx) = mpsc::unbounded_channel::<(String, Value)>();
        let hook_router = Router::new().route(
            "/hook",
            post({
                let hook_tx = hook_tx.clone();
                move |headers: HeaderMap, body: Bytes| {
                    let hook_tx = hook_tx.clone();
                    async move {
                        let sig = headers
                            .get("x-aircd-signature-256")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or_default()
                            .to_string();
                        let json_val: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                        let _ = hook_tx.send((sig, json_val));
                        StatusCode::OK
                    }
                }
            }),
        );
        let hook_listener = TcpListener::bind("127.0.0.1:0").await?;
        let hook_addr = hook_listener.local_addr()?;
        tokio::spawn(async move {
            let _ = axum::serve(hook_listener, hook_router).await;
        });

        let api_listener = TcpListener::bind("127.0.0.1:0").await?;
        let api_addr = api_listener.local_addr()?;
        let info = HttpRuntimeInfo {
            irc_bind: "127.0.0.1:6667".to_string(),
            irc_port: 6667,
            http_bind: api_addr.to_string(),
            http_port: api_addr.port(),
            tls_port: None,
            detected_hosts: vec!["127.0.0.1".to_string()],
            register_key: None,
        };
        tokio::spawn(async move {
            let _ = axum::serve(api_listener, router(server, info)).await;
        });

        let client = reqwest::Client::new();
        let base = format!("http://{api_addr}");

        // 1. Public welcome page renders cleanly and omits tokens.
        let welcome_html = client.get(&base).send().await?.text().await?;
        assert!(welcome_html.contains("aircd"));
        assert!(!welcome_html.contains("human-token"));

        let welcome_json: Value = client
            .get(format!("{base}/v1/welcome"))
            .send()
            .await?
            .json()
            .await?;
        assert_eq!(welcome_json["ok"], true);

        // 2. Register codex + claude; duplicate nick gets 409 Conflict.
        let reg1: Value = client
            .post(format!("{base}/v1/register"))
            .json(&json!({"nick": "codex", "role": "leader", "channels": ["#wardrobe"]}))
            .send()
            .await?
            .json()
            .await?;
        let codex_token = reg1["token"].as_str().expect("codex token").to_string();

        let dup_resp = client
            .post(format!("{base}/v1/register"))
            .json(&json!({"nick": "CODEX"}))
            .send()
            .await?;
        assert_eq!(dup_resp.status(), StatusCode::CONFLICT);

        let reg2: Value = client
            .post(format!("{base}/v1/register"))
            .json(&json!({"nick": "claude", "role": "reviewer", "channels": ["#wardrobe"]}))
            .send()
            .await?
            .json()
            .await?;
        let claude_token = reg2["token"].as_str().expect("claude token").to_string();

        // 3. Configure webhook on claude and test ping.
        let hook_url = format!("http://{hook_addr}/hook");
        let wh_set: Value = client
            .post(format!("{base}/v1/webhooks"))
            .bearer_auth(&claude_token)
            .json(&json!({"url": hook_url, "notify_when": "always"}))
            .send()
            .await?
            .json()
            .await?;
        let secret = wh_set["secret"]
            .as_str()
            .expect("webhook secret")
            .to_string();

        let test_res = client
            .post(format!("{base}/v1/webhooks/test"))
            .bearer_auth(&claude_token)
            .send()
            .await?;
        assert_eq!(test_res.status(), StatusCode::OK);
        let (_sig, ping_body) = hook_rx.recv().await.expect("test webhook delivered");
        assert_eq!(ping_body["event"], "connection_test");

        // 4. Codex sends mention message -> webhook delivers signed event to claude.
        let msg_res = client
            .post(format!("{base}/v1/messages"))
            .bearer_auth(&codex_token)
            .json(&json!({"target": "#wardrobe", "body": "@claude please review PR #9"}))
            .send()
            .await?;
        assert_eq!(msg_res.status(), StatusCode::CREATED);
        let (sig, msg_body) = hook_rx.recv().await.expect("mention webhook delivered");
        assert!(sig.starts_with("sha256="));
        assert_eq!(msg_body["event"], "mention");
        assert_eq!(msg_body["from"], "codex");

        // 5. Dev equipment lock + conflict check.
        let lock_ok = client
            .post(format!("{base}/v1/locks"))
            .bearer_auth(&codex_token)
            .json(&json!({"resource": "quest-1", "channel": "#wardrobe", "ttl_seconds": 300}))
            .send()
            .await?;
        assert_eq!(lock_ok.status(), StatusCode::OK);
        let _ = hook_rx.recv().await;

        let lock_conflict = client
            .post(format!("{base}/v1/locks"))
            .bearer_auth(&claude_token)
            .json(&json!({"resource": "quest-1", "channel": "#wardrobe"}))
            .send()
            .await?;
        assert_eq!(lock_conflict.status(), StatusCode::CONFLICT);

        // 6. GitHub webhook signature verification & channel broadcast.
        let git_reg: Value = client
            .post(format!("{base}/v1/gits"))
            .bearer_auth(&codex_token)
            .json(&json!({
                "name": "wardrobe",
                "channel": "#wardrobe",
                "local_path": "C:/work/wardrobe"
            }))
            .send()
            .await?
            .json()
            .await?;
        let gh_secret = git_reg["webhook_secret"]
            .as_str()
            .expect("git webhook secret")
            .to_string();
        assert!(!secret.is_empty());

        let gh_payload = br#"{"ref":"refs/heads/feat-ui","after":"abcdef123456","sender":{"login":"codex"},"head_commit":{"id":"abcdef123456","message":"polish layout"}}"#;
        let gh_sig = sign_payload(&gh_secret, gh_payload);
        let gh_ok = client
            .post(format!("{base}/v1/git/webhook/wardrobe"))
            .header("X-GitHub-Event", "push")
            .header("X-Hub-Signature-256", gh_sig)
            .header("Content-Type", "application/json")
            .body(gh_payload.to_vec())
            .send()
            .await?;
        assert_eq!(gh_ok.status(), StatusCode::OK);

        let gh_bad = client
            .post(format!("{base}/v1/git/webhook/wardrobe"))
            .header("X-GitHub-Event", "push")
            .header("X-Hub-Signature-256", "sha256=deadbeef")
            .header("Content-Type", "application/json")
            .body(gh_payload.to_vec())
            .send()
            .await?;
        assert_eq!(gh_bad.status(), StatusCode::UNAUTHORIZED);

        Ok(())
    }

    /// POST/GET helper for the project and task flow test.
    async fn call(
        client: &reqwest::Client,
        method: &str,
        url: String,
        token: &str,
        body: Value,
    ) -> Result<(u16, Value)> {
        let request = match method {
            "GET" => client.get(url),
            _ => client.post(url).json(&body),
        };
        let response = request.bearer_auth(token).send().await?;
        let status = response.status().as_u16();
        Ok((status, response.json().await.unwrap_or(Value::Null)))
    }

    #[tokio::test]
    async fn project_tasks_stay_in_their_project_and_go_through_review() -> Result<()> {
        let db = store::open(":memory:")?;
        let server = Server::new(db);
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let info = build_runtime_info("127.0.0.1:6667", &addr.to_string(), None);
        tokio::spawn(async move {
            let _ = axum::serve(listener, router(server, info)).await;
        });
        let client = reqwest::Client::new();
        let base = format!("http://{addr}");

        let mut tokens = std::collections::HashMap::new();
        for (nick, role) in [
            ("owner", "leader"),
            ("worker", "member"),
            ("coord", "member"),
            ("outsider", "member"),
        ] {
            let res: Value = client
                .post(format!("{base}/v1/register"))
                .json(&json!({"nick": nick, "role": role}))
                .send()
                .await?
                .json()
                .await?;
            tokens.insert(nick, res["token"].as_str().unwrap().to_string());
        }
        let (owner, worker, coord, outsider) = (
            tokens["owner"].as_str(),
            tokens["worker"].as_str(),
            tokens["coord"].as_str(),
            tokens["outsider"].as_str(),
        );

        // #general is chat only: no project, no tasks.
        let (status, _) = call(&client, "POST", format!("{base}/v1/projects"), owner,
            json!({"channel": "#general"})).await?;
        assert_eq!(status, 400);
        let (status, _) = call(&client, "POST", format!("{base}/v1/tasks"), owner,
            json!({"channel": "#general", "title": "nope"})).await?;
        assert_eq!(status, 400);
        let (status, _) = call(&client, "POST", format!("{base}/v1/tasks"), owner,
            json!({"title": "no channel"})).await?;
        assert_eq!(status, 400);

        // Only a leader or coordinator creates a project.
        let (status, _) = call(&client, "POST", format!("{base}/v1/projects"), worker,
            json!({"channel": "#quest"})).await?;
        assert_eq!(status, 400);
        let (status, body) = call(&client, "POST", format!("{base}/v1/projects"), owner,
            json!({"channel": "quest", "name": "Quest player"})).await?;
        assert_eq!(status, 201);
        assert_eq!(body["project"]["channel"], "#quest");

        for (nick, role) in [("worker", "member"), ("coord", "coordinator")] {
            let (status, _) = call(&client, "POST", format!("{base}/v1/projects/members"), owner,
                json!({"channel": "#quest", "nick": nick, "role": role})).await?;
            assert_eq!(status, 200);
        }

        // Work cannot be assigned to an account outside the project,
        // and a plain member cannot assign work to someone else.
        let (status, body) = call(&client, "POST", format!("{base}/v1/tasks"), owner,
            json!({"channel": "#quest", "title": "t", "assignee": "outsider"})).await?;
        assert_eq!(status, 400);
        assert!(body["error"].as_str().unwrap().contains("not a member"));
        let (status, _) = call(&client, "POST", format!("{base}/v1/tasks"), worker,
            json!({"channel": "#quest", "title": "t", "assignee": "coord"})).await?;
        assert_eq!(status, 400);

        let (status, body) = call(&client, "POST", format!("{base}/v1/tasks"), owner, json!({
            "channel": "#quest", "title": "Fix seek bar", "kind": "bug",
            "body": "Seek bar jumps back after scrubbing.", "assignee": "@worker",
            "repo": "quest-player",
        })).await?;
        assert_eq!(status, 201);
        let task = &body["task"];
        assert_eq!(task["status"], "assigned");
        assert_eq!(task["assignee"], "worker");
        assert_eq!(task["kind"], "bug");
        let id = task["id"].as_str().unwrap().to_string();

        // The assignee's inbox names the project the task belongs to.
        let (_, body) = call(&client, "GET", format!("{base}/v1/inbox"), worker, Value::Null).await?;
        assert_eq!(body["inbox"]["task_count"], 1);
        assert_eq!(body["inbox"]["projects"][0]["channel"], "#quest");
        assert_eq!(body["inbox"]["projects"][0]["tasks"][0]["id"], id.as_str());

        // Only the assignee moves work forward; only a manager decides review.
        let status_url = format!("{base}/v1/tasks/{id}/status");
        let (status, _) = call(&client, "POST", status_url.clone(), outsider,
            json!({"status": "in_progress"})).await?;
        assert_eq!(status, 400);
        let (status, _) = call(&client, "POST", status_url.clone(), worker,
            json!({"status": "in_progress"})).await?;
        assert_eq!(status, 200);
        let (status, body) = call(&client, "POST", status_url.clone(), worker,
            json!({"status": "review", "branch": "fix/seek-bar"})).await?;
        assert_eq!(status, 200);
        assert_eq!(body["task"]["branch"], "fix/seek-bar");
        let (status, _) = call(&client, "POST", status_url.clone(), worker,
            json!({"status": "merged"})).await?;
        assert_eq!(status, 400);

        let (_, body) = call(&client, "GET", format!("{base}/v1/inbox"), coord, Value::Null).await?;
        assert_eq!(body["inbox"]["review_count"], 1);

        let (status, _) = call(&client, "POST", status_url.clone(), coord,
            json!({"status": "changes-requested", "note": "add a test"})).await?;
        assert_eq!(status, 200);
        let (status, _) = call(&client, "POST", status_url.clone(), worker,
            json!({"status": "review"})).await?;
        assert_eq!(status, 200);
        let (status, body) = call(&client, "POST", status_url.clone(), coord,
            json!({"status": "merged"})).await?;
        assert_eq!(status, 200);
        assert_eq!(body["task"]["status"], "merged");

        let (_, body) = call(&client, "GET", format!("{base}/v1/inbox"), worker, Value::Null).await?;
        assert_eq!(body["inbox"]["task_count"], 0);
        let (_, body) = call(&client, "GET", format!("{base}/v1/tasks/{id}"), worker, Value::Null).await?;
        assert_eq!(body["events"].as_array().unwrap().len(), 5);

        // Reassignment moves the task to the new account's inbox.
        let (_, body) = call(&client, "POST", format!("{base}/v1/tasks"), owner,
            json!({"channel": "#quest", "title": "Second", "assignee": "worker"})).await?;
        let second = body["task"]["id"].as_str().unwrap().to_string();
        let (status, body) = call(&client, "POST", format!("{base}/v1/tasks/{second}/assign"), coord,
            json!({"assignee": "coord"})).await?;
        assert_eq!(status, 200);
        assert_eq!(body["task"]["assignee"], "coord");
        assert_eq!(body["task"]["status"], "assigned");
        let (_, body) = call(&client, "GET", format!("{base}/v1/inbox"), worker, Value::Null).await?;
        assert_eq!(body["inbox"]["task_count"], 0);
        let (_, body) = call(&client, "GET", format!("{base}/v1/inbox"), coord, Value::Null).await?;
        assert_eq!(body["inbox"]["task_count"], 1);

        Ok(())
    }

    #[tokio::test]
    async fn assigning_a_task_launches_the_assignees_configured_command() -> Result<()> {
        let db = store::open(":memory:")?;
        let server = Server::new(db);
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let mut info = build_runtime_info("127.0.0.1:6667", &addr.to_string(), None);
        info.register_key = Some("owner-key".to_string());
        tokio::spawn(async move {
            let _ = axum::serve(listener, router(server, info)).await;
        });
        let client = reqwest::Client::new();
        let base = format!("http://{addr}");

        let mut tokens = std::collections::HashMap::new();
        for (nick, role) in [("owner", "leader"), ("worker", "member")] {
            let res: Value = client
                .post(format!("{base}/v1/register"))
                .json(&json!({"nick": nick, "role": role, "invite": "owner-key"}))
                .send()
                .await?
                .json()
                .await?;
            tokens.insert(nick, res["token"].as_str().unwrap().to_string());
        }
        let owner = tokens["owner"].as_str();
        let wake_url = format!("{base}/v1/owner/wake");

        // The command prints a marker built from the placeholders; no shell
        // quoting is involved, so it behaves the same on every platform.
        let (command, args) = if cfg!(windows) {
            ("cmd", json!(["/C", "echo", "woke-{nick}-{task_id}"]))
        } else {
            ("echo", json!(["woke-{nick}-{task_id}"]))
        };
        let config = json!({
            "nick": "worker", "command": command, "args": args,
            "cooldown_seconds": 0, "timeout_seconds": 30,
        });

        // Configuring a launcher needs the owner key: it runs a command.
        let (status, _) = call(&client, "POST", wake_url.clone(), owner, config.clone()).await?;
        assert_eq!(status, 403);
        let mut keyed = config.clone();
        keyed["invite"] = json!("owner-key");
        let (status, body) = call(&client, "POST", wake_url.clone(), "", keyed).await?;
        assert_eq!(status, 200);
        assert_eq!(body["wake"]["enabled"], true);

        call(&client, "POST", format!("{base}/v1/projects"), owner,
            json!({"channel": "#quest"})).await?;
        call(&client, "POST", format!("{base}/v1/projects/members"), owner,
            json!({"channel": "#quest", "nick": "worker"})).await?;
        let (status, body) = call(&client, "POST", format!("{base}/v1/tasks"), owner,
            json!({"channel": "#quest", "title": "Wake me", "assignee": "worker"})).await?;
        assert_eq!(status, 201);
        let task_id = body["task"]["id"].as_str().unwrap().to_string();

        // The assignment itself starts the worker's command.
        let mut finished = None;
        for _ in 0..100 {
            let (_, body) = call(&client, "POST", wake_url.clone(), "",
                json!({"invite": "owner-key", "action": "runs", "nick": "worker"})).await?;
            if let Some(run) = body["runs"].as_array().and_then(|runs| runs.first()) {
                if run["status"] != "running" {
                    finished = Some(run.clone());
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let run = finished.expect("the wake run should finish");
        assert_eq!(run["status"], "ok", "run: {run}");
        assert_eq!(run["task_id"], task_id.as_str());
        assert!(run["log_tail"]
            .as_str()
            .unwrap()
            .contains(&format!("woke-worker-{task_id}")));

        // An account with no launcher is simply not started.
        let (_, body) = call(&client, "POST", wake_url.clone(), "",
            json!({"invite": "owner-key", "action": "test", "nick": "owner"})).await?;
        assert_eq!(body["started"], false);

        Ok(())
    }

    #[tokio::test]
    async fn a_resume_launcher_continues_the_stored_conversation() -> Result<()> {
        let db = store::open(":memory:")?;
        let server = Server::new(db);
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let mut info = build_runtime_info("127.0.0.1:6667", &addr.to_string(), None);
        info.register_key = Some("owner-key".to_string());
        tokio::spawn(async move {
            let _ = axum::serve(listener, router(server, info)).await;
        });
        let client = reqwest::Client::new();
        let base = format!("http://{addr}");
        client
            .post(format!("{base}/v1/register"))
            .json(&json!({"nick": "pm", "invite": "owner-key"}))
            .send()
            .await?;
        let wake_url = format!("{base}/v1/owner/wake");

        // A stand-in agent: the first launch prints a conversation id, later
        // launches are started with that id.
        let (command, fresh, resumed) = if cfg!(windows) {
            (
                "cmd",
                json!(["/C", "echo", "fresh", "session_id:", "conv-777"]),
                json!(["/C", "echo", "resumed-{session_id}"]),
            )
        } else {
            (
                "echo",
                json!(["fresh", "session_id:", "conv-777"]),
                json!(["resumed-{session_id}"]),
            )
        };
        let (status, body) = call(&client, "POST", wake_url.clone(), "", json!({
            "invite": "owner-key", "nick": "pm", "command": command, "args": fresh,
            "resume_args": resumed, "session_mode": "resume",
            "session_marker": "session_id:", "cooldown_seconds": 0,
        })).await?;
        assert_eq!(status, 200);
        assert_eq!(body["wake"]["session_mode"], "resume");

        let mut logs = Vec::new();
        for round in 0..3 {
            if round == 2 {
                // Forgetting the conversation makes the next launch fresh again.
                let (_, body) = call(&client, "POST", wake_url.clone(), "",
                    json!({"invite": "owner-key", "action": "reset", "nick": "pm"})).await?;
                assert_eq!(body["removed"], 1);
            }
            call(&client, "POST", wake_url.clone(), "",
                json!({"invite": "owner-key", "action": "test", "nick": "pm"})).await?;
            let mut done = None;
            for _ in 0..100 {
                let (_, body) = call(&client, "POST", wake_url.clone(), "",
                    json!({"invite": "owner-key", "action": "runs", "nick": "pm"})).await?;
                let runs = body["runs"].as_array().cloned().unwrap_or_default();
                if runs.len() == round + 1 && runs.iter().all(|r| r["status"] != "running") {
                    done = runs.first().cloned();
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            let run = done.expect("the wake run should finish");
            assert_eq!(run["status"], "ok", "run: {run}");
            logs.push(run["log_tail"].as_str().unwrap().to_string());
            // Let the launcher release the account before the next round.
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        }
        assert!(logs[0].contains("fresh"), "{logs:?}");
        assert!(logs[1].contains("resumed-conv-777"), "{logs:?}");
        assert!(logs[2].contains("fresh"), "{logs:?}");

        Ok(())
    }

    #[tokio::test]
    async fn blocked_questions_go_to_the_pm_then_the_owner_and_notes_persist() -> Result<()> {
        let db = store::open(":memory:")?;
        let server = Server::new(db);
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let info = build_runtime_info("127.0.0.1:6667", &addr.to_string(), None);
        tokio::spawn(async move {
            let _ = axum::serve(listener, router(server, info)).await;
        });
        let client = reqwest::Client::new();
        let base = format!("http://{addr}");

        let mut tokens = std::collections::HashMap::new();
        for (nick, role) in [
            ("owner", "leader"),
            ("worker", "member"),
            ("pm1", "member"),
            ("pm2", "member"),
        ] {
            let res: Value = client
                .post(format!("{base}/v1/register"))
                .json(&json!({"nick": nick, "role": role}))
                .send()
                .await?
                .json()
                .await?;
            tokens.insert(nick, res["token"].as_str().unwrap().to_string());
        }
        let (owner, worker, pm1, pm2) = (
            tokens["owner"].as_str(),
            tokens["worker"].as_str(),
            tokens["pm1"].as_str(),
            tokens["pm2"].as_str(),
        );

        call(&client, "POST", format!("{base}/v1/projects"), owner,
            json!({"channel": "#quest"})).await?;
        call(&client, "POST", format!("{base}/v1/projects/members"), owner,
            json!({"channel": "#quest", "nick": "worker"})).await?;

        // Only the owner chooses the PM; there is one per project.
        let pm_url = format!("{base}/v1/projects/pm");
        let (status, _) = call(&client, "POST", pm_url.clone(), worker,
            json!({"channel": "#quest", "nick": "worker"})).await?;
        assert_eq!(status, 400);
        let (status, body) = call(&client, "POST", pm_url.clone(), owner,
            json!({"channel": "#quest", "nick": "pm1"})).await?;
        assert_eq!(status, 200);
        assert_eq!(body["project"]["pm"], "pm1");
        let (_, body) = call(&client, "POST", pm_url.clone(), owner,
            json!({"channel": "#quest", "nick": "pm2"})).await?;
        assert_eq!(body["project"]["pm"], "pm2");
        let (_, body) = call(&client, "GET",
            format!("{base}/v1/projects/members?channel=%23quest"), owner, Value::Null).await?;
        let role_of = |nick: &str| {
            body["members"].as_array().unwrap().iter()
                .find(|m| m["principal_id"] == nick).map(|m| m["role"].clone())
        };
        assert_eq!(role_of("pm2"), Some(json!("coordinator")));
        assert_eq!(role_of("pm1"), Some(json!("member")), "a replaced PM steps back");

        let (_, body) = call(&client, "POST", format!("{base}/v1/tasks"), owner,
            json!({"channel": "#quest", "title": "Pick a codec", "assignee": "worker"})).await?;
        let id = body["task"]["id"].as_str().unwrap().to_string();
        let task_url = |action: &str| format!("{base}/v1/tasks/{id}/{action}");
        call(&client, "POST", task_url("status"), worker, json!({"status": "in_progress"})).await?;

        // Only the assignee asks; the task is parked as blocked.
        let (status, _) = call(&client, "POST", task_url("ask"), pm2,
            json!({"question": "?"})).await?;
        assert_eq!(status, 400);
        let (status, body) = call(&client, "POST", task_url("ask"), worker,
            json!({"question": "H.264 or AV1?"})).await?;
        assert_eq!(status, 200);
        assert_eq!(body["task"]["status"], "blocked");

        // The PM sees the question; a plain member does not answer it.
        let (_, body) = call(&client, "GET", format!("{base}/v1/inbox"), pm2, Value::Null).await?;
        assert_eq!(body["inbox"]["question_count"], 1);
        assert_eq!(body["inbox"]["projects"][0]["questions"][0]["question"], "H.264 or AV1?");
        assert_eq!(body["inbox"]["projects"][0]["pm"], "pm2");
        let (status, _) = call(&client, "POST", task_url("answer"), pm1,
            json!({"answer": "AV1"})).await?;
        assert_eq!(status, 400);

        // The PM answers: the task goes back to where it was.
        let (status, body) = call(&client, "POST", task_url("answer"), pm2,
            json!({"answer": "H.264 for now"})).await?;
        assert_eq!(status, 200);
        assert_eq!(body["task"]["status"], "in_progress");

        // A second question the PM cannot decide goes up to the owner.
        call(&client, "POST", task_url("ask"), worker,
            json!({"question": "May I buy a licence?"})).await?;
        let (_, body) = call(&client, "GET", format!("{base}/v1/inbox"), owner, Value::Null).await?;
        assert_eq!(body["inbox"]["needs_you"].as_array().unwrap().len(), 0);
        let (status, _) = call(&client, "POST", task_url("escalate"), pm2,
            json!({"note": "budget decision"})).await?;
        assert_eq!(status, 200);
        let (_, body) = call(&client, "GET", format!("{base}/v1/inbox"), owner, Value::Null).await?;
        assert_eq!(body["inbox"]["needs_you"][0]["question"], "May I buy a licence?");
        assert_eq!(body["inbox"]["needs_you"][0]["escalation_note"], "budget decision");
        let (_, body) = call(&client, "GET", format!("{base}/v1/inbox"), pm2, Value::Null).await?;
        assert_eq!(body["inbox"]["question_count"], 0, "escalated questions leave the PM's list");
        let (status, body) = call(&client, "POST", task_url("answer"), owner,
            json!({"answer": "Yes, up to $50"})).await?;
        assert_eq!(status, 200);
        assert_eq!(body["task"]["status"], "in_progress");
        let (_, body) = call(&client, "GET", format!("{base}/v1/inbox"), owner, Value::Null).await?;
        assert_eq!(body["inbox"]["needs_you"].as_array().unwrap().len(), 0);

        // Notes: an account's own memory; only the owner reads someone else's.
        let notes_url = format!("{base}/v1/notes");
        call(&client, "POST", notes_url.clone(), worker, json!({"body": "Codec: H.264"})).await?;
        let (_, body) = call(&client, "POST", notes_url.clone(), worker,
            json!({"body": "Licence budget $50", "append": true})).await?;
        assert_eq!(body["notes"]["body"], "Codec: H.264\nLicence budget $50");
        let (_, body) = call(&client, "GET", notes_url.clone(), worker, Value::Null).await?;
        assert_eq!(body["notes"]["body"], "Codec: H.264\nLicence budget $50");
        let (status, _) = call(&client, "GET", format!("{notes_url}?nick=worker"), pm2, Value::Null).await?;
        assert_eq!(status, 403);
        let (status, body) = call(&client, "GET", format!("{notes_url}?nick=worker"), owner, Value::Null).await?;
        assert_eq!(status, 200);
        assert_eq!(body["notes"]["principal_id"], "worker");

        Ok(())
    }

    #[tokio::test]
    async fn a_pm_launcher_resumes_only_when_woken_as_the_project_manager() -> Result<()> {
        let db = store::open(":memory:")?;
        let server = Server::new(db);
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let mut info = build_runtime_info("127.0.0.1:6667", &addr.to_string(), None);
        info.register_key = Some("owner-key".to_string());
        tokio::spawn(async move {
            let _ = axum::serve(listener, router(server, info)).await;
        });
        let client = reqwest::Client::new();
        let base = format!("http://{addr}");
        let mut tokens = std::collections::HashMap::new();
        for (nick, role) in [("owner", "leader"), ("flex", "member")] {
            let res: Value = client
                .post(format!("{base}/v1/register"))
                .json(&json!({"nick": nick, "role": role, "invite": "owner-key"}))
                .send()
                .await?
                .json()
                .await?;
            tokens.insert(nick, res["token"].as_str().unwrap().to_string());
        }
        let owner = tokens["owner"].as_str();
        for channel in ["#managed", "#other"] {
            call(&client, "POST", format!("{base}/v1/projects"), owner,
                json!({"channel": channel})).await?;
        }
        call(&client, "POST", format!("{base}/v1/projects/pm"), owner,
            json!({"channel": "#managed", "nick": "flex"})).await?;

        let wake_url = format!("{base}/v1/owner/wake");
        let (command, fresh, resumed) = if cfg!(windows) {
            (
                "cmd",
                json!(["/C", "echo", "fresh-{role}", "session_id:", "conv-9"]),
                json!(["/C", "echo", "resumed-{session_id}"]),
            )
        } else {
            (
                "echo",
                json!(["fresh-{role}", "session_id:", "conv-9"]),
                json!(["resumed-{session_id}"]),
            )
        };
        call(&client, "POST", wake_url.clone(), "", json!({
            "invite": "owner-key", "nick": "flex", "command": command, "args": fresh,
            "resume_args": resumed, "session_mode": "pm",
            "session_marker": "session_id:", "cooldown_seconds": 0,
        })).await?;

        let mut logs = Vec::new();
        for (round, channel) in ["#managed", "#managed", "#other"].into_iter().enumerate() {
            call(&client, "POST", wake_url.clone(), "", json!({
                "invite": "owner-key", "action": "test", "nick": "flex", "channel": channel,
            })).await?;
            let mut done = None;
            for _ in 0..100 {
                let (_, body) = call(&client, "POST", wake_url.clone(), "",
                    json!({"invite": "owner-key", "action": "runs", "nick": "flex"})).await?;
                let runs = body["runs"].as_array().cloned().unwrap_or_default();
                if runs.len() == round + 1 && runs.iter().all(|r| r["status"] != "running") {
                    done = runs.first().cloned();
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            let run = done.expect("the wake run should finish");
            logs.push(run["log_tail"].as_str().unwrap().to_string());
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        }
        assert!(logs[0].contains("fresh-project"), "first PM launch starts the conversation: {logs:?}");
        assert!(logs[1].contains("resumed-conv-9"), "second PM launch resumes it: {logs:?}");
        assert!(logs[2].contains("fresh-member"), "as a plain member it starts fresh: {logs:?}");

        Ok(())
    }

    #[tokio::test]
    async fn register_requires_the_invite_key_when_one_is_set() -> Result<()> {
        let db = store::open(":memory:")?;
        let server = Server::new(db);
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let mut info = build_runtime_info("127.0.0.1:6667", &addr.to_string(), None);
        info.register_key = Some("owner-key-123".to_string());
        tokio::spawn(async move {
            let _ = axum::serve(listener, router(server, info)).await;
        });

        let client = reqwest::Client::new();
        let url = format!("http://{addr}/v1/register");

        let missing = client
            .post(&url)
            .json(&json!({"nick": "intruder", "role": "leader"}))
            .send()
            .await?;
        assert_eq!(missing.status().as_u16(), 403);

        let wrong = client
            .post(&url)
            .json(&json!({"nick": "intruder", "invite": "guess"}))
            .send()
            .await?;
        assert_eq!(wrong.status().as_u16(), 403);

        let body_ok = client
            .post(&url)
            .json(&json!({"nick": "worker-1", "invite": "owner-key-123"}))
            .send()
            .await?;
        assert_eq!(body_ok.status().as_u16(), 201);

        let header_ok = client
            .post(&url)
            .header("X-AirCD-Invite", "owner-key-123")
            .json(&json!({"nick": "worker-2"}))
            .send()
            .await?;
        assert_eq!(header_ok.status().as_u16(), 201);

        Ok(())
    }

    #[tokio::test]
    async fn owner_can_reissue_a_token_and_the_old_one_stops_working() -> Result<()> {
        let db = store::open(":memory:")?;
        let server = Server::new(db);
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let mut info = build_runtime_info("127.0.0.1:6667", &addr.to_string(), None);
        info.register_key = Some("owner-key-123".to_string());
        tokio::spawn(async move {
            let _ = axum::serve(listener, router(server, info)).await;
        });

        let client = reqwest::Client::new();
        let base = format!("http://{addr}");
        let me = |token: String| {
            let client = client.clone();
            let url = format!("{base}/v1/me");
            async move {
                client
                    .get(url)
                    .bearer_auth(token)
                    .send()
                    .await
                    .map(|r| r.status().as_u16())
            }
        };

        let registered: Value = client
            .post(format!("{base}/v1/register"))
            .json(&json!({"nick": "worker-1", "invite": "owner-key-123"}))
            .send()
            .await?
            .json()
            .await?;
        let old_token = registered["token"].as_str().unwrap().to_string();
        assert_eq!(me(old_token.clone()).await?, 200);

        let reissue = format!("{base}/v1/owner/reissue");
        let missing = client
            .post(&reissue)
            .json(&json!({"nick": "worker-1"}))
            .send()
            .await?;
        assert_eq!(missing.status().as_u16(), 403);
        let wrong = client
            .post(&reissue)
            .json(&json!({"nick": "worker-1", "invite": "guess"}))
            .send()
            .await?;
        assert_eq!(wrong.status().as_u16(), 403);
        assert_eq!(me(old_token.clone()).await?, 200, "failed attempts change nothing");

        let unknown = client
            .post(&reissue)
            .json(&json!({"nick": "nobody", "invite": "owner-key-123"}))
            .send()
            .await?;
        assert_eq!(unknown.status().as_u16(), 404);

        let ok = client
            .post(&reissue)
            .json(&json!({"nick": "@Worker-1", "invite": "owner-key-123"}))
            .send()
            .await?;
        assert_eq!(ok.status().as_u16(), 200);
        let body: Value = ok.json().await?;
        assert_eq!(body["principal"]["id"], "worker-1");
        let new_token = body["token"].as_str().unwrap().to_string();
        assert_ne!(new_token, old_token);
        assert_eq!(me(old_token.clone()).await?, 401);
        assert_eq!(me(new_token.clone()).await?, 200);

        // A second reissue (owner key in the header) works and retires the first.
        let again = client
            .post(&reissue)
            .header("X-AirCD-Invite", "owner-key-123")
            .json(&json!({"nick": "worker-1"}))
            .send()
            .await?;
        assert_eq!(again.status().as_u16(), 200);
        let newest = again.json::<Value>().await?["token"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(me(new_token).await?, 401);
        assert_eq!(me(newest).await?, 200);

        // Seeded legacy plaintext tokens are neutralised as well.
        assert_eq!(me("agent-a-token".to_string()).await?, 200);
        let legacy = client
            .post(&reissue)
            .json(&json!({"nick": "agent-a", "invite": "owner-key-123"}))
            .send()
            .await?;
        assert_eq!(legacy.status().as_u16(), 200);
        assert_eq!(me("agent-a-token".to_string()).await?, 401);

        Ok(())
    }

    #[tokio::test]
    async fn reissue_is_refused_when_no_owner_key_is_configured() -> Result<()> {
        let db = store::open(":memory:")?;
        let server = Server::new(db);
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let info = build_runtime_info("127.0.0.1:6667", &addr.to_string(), None);
        tokio::spawn(async move {
            let _ = axum::serve(listener, router(server, info)).await;
        });

        let refused = reqwest::Client::new()
            .post(format!("http://{addr}/v1/owner/reissue"))
            .json(&json!({"nick": "agent-a", "invite": ""}))
            .send()
            .await?;
        assert_eq!(refused.status().as_u16(), 403);
        Ok(())
    }

    #[test]
    fn runtime_info_lists_only_reachable_hosts() {
        let local = build_runtime_info("127.0.0.1:6667", "127.0.0.1:6680", None);
        assert_eq!(local.detected_hosts, vec!["127.0.0.1".to_string()]);
        let wildcard = build_runtime_info("0.0.0.0:6667", "0.0.0.0:6680", None);
        assert!(wildcard.detected_hosts.contains(&"127.0.0.1".to_string()));
    }
}
