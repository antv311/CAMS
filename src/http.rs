use anyhow::{Context, Result};
use axum::body::Bytes;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use tokio::net::TcpListener;

use crate::auth::{
    AuthContext, DuplicatePrincipalError, DuplicateTokenLabelError, Scope, TokenExpiredError,
};
use crate::gits::{sanitize_repo_name, verify_github_signature};
use crate::locks::LockConflictError;
use crate::logging::{LogEvent, Severity};
use crate::rate_limit::{
    normalize_ip, LimitScope, RateLimitConfig, RateLimitHeaders, RateLimitRejection, RateLimiter,
};
use crate::server::{Server, SpawnError};
use crate::store::{
    ClientTemplate, ClientTemplateInput, GroupInput, GroupMember, GroupMembershipUpdate,
    GroupMemoryError, GroupStoreError, WalCheckpointMode,
};

#[derive(Clone, Debug, Serialize)]
pub struct HttpRuntimeInfo {
    pub irc_bind: String,
    pub irc_port: u16,
    pub http_bind: String,
    pub http_port: u16,
    pub http_tls_port: Option<u16>,
    pub tls_port: Option<u16>,
    pub tls_client_auth: String,
    pub tls_only: bool,
    pub detected_hosts: Vec<String>,
    /// Owner invite key. When set, `POST /v1/register` must present it.
    #[serde(skip)]
    pub register_key: Option<String>,
    /// Token-bucket HTTP rate-limiting policy.
    #[serde(skip)]
    pub rate_limit: RateLimitConfig,
}

#[derive(Clone)]
struct AppState {
    server: Server,
    info: HttpRuntimeInfo,
    rate_limiter: RateLimiter,
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

    fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, message)
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

#[derive(Clone, Debug)]
struct InternalErrorDetail(String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = (
            self.status,
            with_cors_headers(),
            Json(json!({ "ok": false, "error": self.message })),
        )
            .into_response();
        if self.status.is_server_error() {
            response
                .extensions_mut()
                .insert(InternalErrorDetail(self.message));
        }
        response
    }
}

#[derive(Clone)]
enum CachedTokenAuth {
    Ok(AuthContext),
    Expired,
    Invalid,
}

tokio::task_local! {
    static REQUEST_TOKEN_AUTH: (String, CachedTokenAuth);
}

fn extract_client_ip(request: &Request, trust_proxy_headers: bool) -> IpAddr {
    if trust_proxy_headers {
        if let Some(forwarded) = request
            .headers()
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
        {
            if let Some(ip) = forwarded
                .split(',')
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .find_map(|part| part.parse::<IpAddr>().ok())
            {
                return normalize_ip(ip);
            }
        }
        if let Some(real_ip) = request
            .headers()
            .get("x-real-ip")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<IpAddr>().ok())
        {
            return normalize_ip(real_ip);
        }
    }
    request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| normalize_ip(ci.0.ip()))
        .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST))
}

fn apply_rate_limit_headers(headers: &mut HeaderMap, rl: RateLimitHeaders) {
    if let Ok(val) = HeaderValue::from_str(&rl.limit.to_string()) {
        headers.insert(HeaderName::from_static("x-ratelimit-limit"), val);
    }
    if let Ok(val) = HeaderValue::from_str(&rl.remaining.to_string()) {
        headers.insert(HeaderName::from_static("x-ratelimit-remaining"), val);
    }
    if let Ok(val) = HeaderValue::from_str(&rl.reset_after_secs.to_string()) {
        headers.insert(HeaderName::from_static("x-ratelimit-reset"), val);
    }
    headers.insert(
        HeaderName::from_static("x-ratelimit-scope"),
        HeaderValue::from_static(rl.scope.as_str()),
    );
}

fn log_rate_limit_rejection(
    state: &AppState,
    rej: RateLimitRejection,
    ip: IpAddr,
    path: &str,
    principal: Option<&crate::auth::Principal>,
) {
    let key = match rej.scope {
        LimitScope::Ip => ip.to_string(),
        LimitScope::Principal => principal
            .map(|p| p.id.clone())
            .unwrap_or_else(|| ip.to_string()),
    };
    let Some(suppressed) = state.rate_limiter.should_log_rejection(rej.scope, &key) else {
        return;
    };
    let suppressed_note = if suppressed > 0 {
        format!(" (suppressed {suppressed} prior rejections)")
    } else {
        String::new()
    };
    let actor = principal
        .map(|p| p.id.clone())
        .unwrap_or_else(|| ip.to_string());
    let who = principal
        .map(|p| format!("principal @{} ({ip})", p.nick))
        .unwrap_or_else(|| format!("ip {ip}"));
    state.server.logger().log_security(
        Severity::Warning,
        "http.rate_limited",
        "failure",
        Some(&actor),
        Some(&ip.to_string()),
        Some(path),
        format!(
            "HTTP rate limit exceeded (scope={}, {who}, path={path}, retry_after={}s){suppressed_note}",
            rej.scope.as_str(),
            rej.retry_after_secs
        ),
    );
}

fn rate_limit_response(
    state: &AppState,
    rej: RateLimitRejection,
    principal: Option<&crate::auth::Principal>,
) -> Response {
    state
        .server
        .metrics()
        .record_http_rate_limited(rej.scope.as_str());
    let target = match rej.scope {
        LimitScope::Ip => "client IP".to_string(),
        LimitScope::Principal => principal
            .map(|p| format!("principal @{}", p.nick))
            .unwrap_or_else(|| "principal".to_string()),
    };
    let message = format!(
        "rate limit exceeded for {target} (retry after {}s)",
        rej.retry_after_secs
    );
    let mut response = (
        StatusCode::TOO_MANY_REQUESTS,
        with_cors_headers(),
        Json(json!({
            "ok": false,
            "error": message,
            "limit_scope": rej.scope.as_str(),
            "retry_after_seconds": rej.retry_after_secs,
        })),
    )
        .into_response();
    let headers = response.headers_mut();
    if let Ok(val) = HeaderValue::from_str(&rej.retry_after_secs.to_string()) {
        headers.insert(header::RETRY_AFTER, val);
    }
    apply_rate_limit_headers(
        headers,
        RateLimitHeaders {
            scope: rej.scope,
            limit: rej.limit,
            remaining: 0,
            reset_after_secs: rej.reset_after_secs,
        },
    );
    response
}

async fn rate_limit_requests(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    if !state.rate_limiter.is_enabled() || request.method() == Method::OPTIONS {
        return next.run(request).await;
    }

    let ip = extract_client_ip(&request, state.rate_limiter.trust_proxy_headers());
    let path = request.uri().path().to_string();
    let raw_token = extract_bearer_token(request.headers()).map(str::to_owned);

    if let Some(raw) = raw_token {
        // Reject already-exhausted IPs before running Argon2 verification.
        if let Err(rej) = state.rate_limiter.peek_ip(ip) {
            log_rate_limit_rejection(&state, rej, ip, &path, None);
            return rate_limit_response(&state, rej, None);
        }

        let auth_outcome = match state.server.auth().authenticate_context(&raw) {
            Ok(ctx) => CachedTokenAuth::Ok(ctx),
            Err(err) if err.downcast_ref::<TokenExpiredError>().is_some() => {
                CachedTokenAuth::Expired
            }
            Err(_) => CachedTokenAuth::Invalid,
        };

        let rl_result = match &auth_outcome {
            CachedTokenAuth::Ok(ctx) => {
                state
                    .rate_limiter
                    .check_principal_and_ip(&ctx.principal.id, ip)
            }
            CachedTokenAuth::Expired | CachedTokenAuth::Invalid => state.rate_limiter.check_ip(ip),
        };

        let principal_ref = match &auth_outcome {
            CachedTokenAuth::Ok(ctx) => Some(&ctx.principal),
            _ => None,
        };

        let rl_headers = match rl_result {
            Ok(headers) => headers,
            Err(rej) => {
                log_rate_limit_rejection(&state, rej, ip, &path, principal_ref);
                return rate_limit_response(&state, rej, principal_ref);
            }
        };

        let mut response = REQUEST_TOKEN_AUTH
            .scope((raw, auth_outcome), next.run(request))
            .await;
        if let Some(rl) = rl_headers {
            apply_rate_limit_headers(response.headers_mut(), rl);
        }
        return response;
    }

    let rl_headers = match state.rate_limiter.check_ip(ip) {
        Ok(headers) => headers,
        Err(rej) => {
            log_rate_limit_rejection(&state, rej, ip, &path, None);
            return rate_limit_response(&state, rej, None);
        }
    };
    let mut response = next.run(request).await;
    if let Some(rl) = rl_headers {
        apply_rate_limit_headers(response.headers_mut(), rl);
    }
    response
}

async fn log_http_server_errors(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let started = std::time::Instant::now();
    let _in_flight = state.server.metrics().inc_http_in_flight();
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let response = next.run(request).await;
    state.server.metrics().record_http_request(
        method.as_str(),
        response.status().as_u16(),
        started.elapsed().as_secs_f64(),
    );
    if response.status().is_server_error() {
        let detail = response
            .extensions()
            .get::<InternalErrorDetail>()
            .map(|entry| entry.0.as_str())
            .unwrap_or("internal server error");
        state.server.logger().log(
            LogEvent::server_error(
                "http.internal_error",
                format!("{method} {path} failed ({}): {detail}", response.status()),
            )
            .with_channel(path),
        );
    }
    response
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
        http_tls_port: None,
        tls_port,
        tls_client_auth: "none".to_string(),
        tls_only: false,
        detected_hosts: reachable_hosts(http_bind),
        register_key: None,
        rate_limit: RateLimitConfig::default(),
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
    axum::serve(
        listener,
        router(server, runtime).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .context("serve HTTP router")
}

pub async fn serve_tls(
    server: Server,
    bind_addr: &str,
    info: HttpRuntimeInfo,
    acceptor: tokio_rustls::TlsAcceptor,
) -> Result<()> {
    let tcp = TcpListener::bind(bind_addr)
        .await
        .with_context(|| format!("bind HTTPS welcome/API listener on {bind_addr}"))?;
    let actual_port = tcp.local_addr()?.port();
    let mut runtime = info;
    runtime.http_tls_port = Some(actual_port);
    let logger = server.logger().clone();
    use axum::serve::ListenerExt;
    let tls_listener = crate::tls::TlsListener::from_tcp(tcp, acceptor, logger)
        .with_context(|| format!("initialize HTTPS TLS listener on {bind_addr}"))?
        .tap_io(|_| {});
    println!(
        "aircd HTTPS welcome & API listening on https://{bind_addr} (port {actual_port}, client_auth={})",
        runtime.tls_client_auth
    );
    axum::serve(
        tls_listener,
        router(server, runtime).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .context("serve HTTPS router")
}

pub fn router(server: Server, info: HttpRuntimeInfo) -> Router {
    let rate_limiter = RateLimiter::new(info.rate_limit.clone())
        .expect("invalid HTTP rate limit configuration");
    let state = AppState {
        server,
        info,
        rate_limiter,
    };
    Router::new()
        .route("/", get(welcome_root))
        .route("/welcome", get(welcome_root))
        .route("/instructions", get(welcome_root))
        .route("/dashboard", get(dashboard_html))
        .route("/dashboard/", get(dashboard_html))
        .route("/dashboard/style.css", get(dashboard_css))
        .route("/dashboard/app.js", get(dashboard_js))
        .route("/v1/welcome", get(welcome_json))
        .route("/v1/dashboard", get(get_dashboard))
        .route("/healthz", get(healthz))
        .route("/metrics", get(prometheus_metrics))
        .route("/v1/agents", get(public_agents))
        .route("/v1/channels", get(public_channels))
        .route("/v1/gits", get(public_gits).post(register_git_repo))
        .route("/v1/locks", get(public_locks).post(acquire_lock))
        .route("/v1/locks/release", post(release_lock_post))
        .route("/v1/locks/{resource}", delete(release_lock_delete))
        .route("/v1/register", post(register_agent))
        .route("/v1/owner/reissue", post(owner_reissue_token))
        .route("/v1/owner/wake", post(owner_wake))
        .route("/v1/owner/models", post(owner_models))
        .route("/v1/owner/accounts", post(owner_create_account))
        .route("/v1/owner/backups", get(owner_list_backups))
        .route("/v1/owner/backup", post(owner_backup_action))
        .route("/v1/owner/db", get(owner_db_status))
        .route("/v1/owner/db/checkpoint", post(owner_db_checkpoint))
        .route("/v1/login", post(login_agent))
        .route("/v1/tokens", get(list_tokens_api).post(issue_token_api))
        .route("/v1/tokens/refresh", post(refresh_token_api))
        .route("/v1/tokens/{token_id}", delete(revoke_token_api))
        .route("/v1/me", get(get_me))
        .route("/v1/templates", get(list_templates))
        .route("/v1/groups", get(list_groups).post(upsert_group))
        .route(
            "/v1/groups/{name}",
            patch(update_group_membership).options(cors_preflight_or_404),
        )
        .route("/v1/groups/{name}/attach", post(attach_group))
        .route("/v1/groups/{name}/heartbeat", post(group_heartbeat))
        .route("/v1/groups/{name}/events", get(group_reassignment_events))
        .route(
            "/v1/groups/{name}/memory/{kind}",
            get(get_group_memory).post(write_group_memory),
        )
        .route("/v1/templates/{id}/spawn", post(spawn_template_worker))
        .route(
            "/v1/templates/{id}",
            get(get_template)
                .post(upsert_template)
                .delete(delete_template),
        )
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
        .route("/v1/tasks/{id}/peer-review", post(peer_review_task))
        .route("/v1/tasks/{id}/ask", post(ask_task))
        .route("/v1/tasks/{id}/answer", post(answer_task))
        .route("/v1/tasks/{id}/escalate", post(escalate_task))
        .route(
            "/v1/tasks/{id}/attachments",
            get(list_task_attachments)
                .post(upload_task_attachment)
                .layer(DefaultBodyLimit::max(
                    crate::attachments::MAX_ATTACHMENT_BYTES + 64 * 1024,
                )),
        )
        .route(
            "/v1/tasks/{id}/attachments/{attachment_id}",
            get(download_task_attachment),
        )
        .route("/v1/notes", get(get_notes).post(write_notes))
        .route("/v1/projects/pm", post(set_project_pm))
        .route("/v1/rooms", get(list_rooms).post(open_room))
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
        .layer(middleware::from_fn_with_state(
            state.clone(),
            rate_limit_requests,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            log_http_server_errors,
        ))
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
            "http_tls_port": state.info.http_tls_port,
            "tls_port": state.info.tls_port,
            "tls_client_auth": state.info.tls_client_auth,
            "tls_only": state.info.tls_only,
        })),
    )
}

async fn prometheus_metrics(State(state): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    let server = state.server.clone();
    let body = tokio::task::spawn_blocking(move || server.render_prometheus_metrics())
        .await
        .map_err(|err| ApiError::internal(anyhow::anyhow!(err)))?
        .map_err(ApiError::internal)?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        [(header::CONTENT_TYPE, crate::metrics::PROMETHEUS_CONTENT_TYPE)],
        body,
    ))
}

async fn dashboard_html() -> impl IntoResponse {
    (
        StatusCode::OK,
        with_cors_headers(),
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        crate::dashboard::DASHBOARD_HTML,
    )
}

async fn dashboard_css() -> impl IntoResponse {
    (
        StatusCode::OK,
        with_cors_headers(),
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        crate::dashboard::DASHBOARD_CSS,
    )
}

async fn dashboard_js() -> impl IntoResponse {
    (
        StatusCode::OK,
        with_cors_headers(),
        [
            (header::CONTENT_TYPE, "application/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        crate::dashboard::DASHBOARD_JS,
    )
}

#[derive(Deserialize)]
struct DashboardQuery {
    channel: Option<String>,
    sprint: Option<String>,
    include_terminal: Option<bool>,
}

async fn get_dashboard(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<DashboardQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let viewer =
        authenticate_scoped_request(&state, &headers, Scope::Read, "/v1/dashboard")?.principal;
    let channel = q
        .channel
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(normalize_http_channel);
    let sprint = q
        .sprint
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(normalize_http_channel);
    let include_terminal = q.include_terminal.unwrap_or(true);
    let server = state.server.clone();
    let snapshot = tokio::task::spawn_blocking(move || {
        crate::dashboard::build_dashboard_snapshot(
            &server,
            &viewer,
            channel.as_deref(),
            sprint.as_deref(),
            include_terminal,
        )
    })
    .await
    .map_err(|err| ApiError::internal(anyhow::anyhow!(err)))?
    .map_err(ApiError::internal)?;
    Ok((StatusCode::OK, with_cors_headers(), Json(snapshot)))
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
    scopes: Option<Vec<String>>,
    ttl_seconds: Option<u64>,
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
            state.server.logger().log_security(
                Severity::Warning,
                "auth.register_denied",
                "failure",
                None,
                Some(&req.nick),
                Some("/v1/register"),
                format!("Registration denied for '{}': invalid owner invite key", req.nick),
            );
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "registration requires the owner's invite key: send it as \"invite\" in the JSON body or in the X-AirCD-Invite header",
            ));
        }
    }
    let (principal, issued) = state
        .server
        .auth()
        .register_principal_with_options(
            &req.nick,
            req.role.as_deref(),
            req.team.as_deref(),
            req.scopes.as_deref(),
            req.ttl_seconds,
        )
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

    state.server.logger().log_security(
        Severity::Info,
        "auth.register_succeeded",
        "success",
        Some(&principal.id),
        Some(&principal.id),
        Some("/v1/register"),
        format!(
            "Registered principal @{} with role '{}'",
            principal.nick, principal.role
        ),
    );

    let host = resolve_request_host(&headers, &state.info);
    Ok((
        StatusCode::CREATED,
        with_cors_headers(),
        Json(json!({
            "ok": true,
            "principal": principal,
            "token": issued.token,
            "token_id": issued.token_id,
            "scopes": issued.scopes,
            "ttl_seconds": issued.ttl_seconds,
            "expires_at": issued.expires_at,
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
        state.server.logger().log_security(
            Severity::Warning,
            "owner.auth_denied",
            "failure",
            None,
            None,
            None,
            "Owner action denied: no owner key is configured",
        );
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
        state.server.logger().log_security(
            Severity::Warning,
            "owner.auth_denied",
            "failure",
            None,
            None,
            None,
            "Owner action denied: invalid owner key",
        );
        Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "this action requires the owner's key: send it as \"invite\" in the JSON body or in the X-AirCD-Invite header",
        ))
    }
}

fn group_store_error(error: anyhow::Error) -> ApiError {
    if let Some(memory_error) = error.downcast_ref::<GroupMemoryError>() {
        return match memory_error {
            GroupMemoryError::Forbidden => ApiError::new(StatusCode::FORBIDDEN, error.to_string()),
            GroupMemoryError::Conflict => ApiError::conflict(error.to_string()),
            GroupMemoryError::Invalid(_) => ApiError::bad_request(error.to_string()),
        };
    }
    if error.downcast_ref::<GroupStoreError>().is_some() {
        ApiError::not_found(error.to_string())
    } else {
        template_store_error(error)
    }
}

async fn get_group_memory(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((name, kind)): Path<(String, String)>,
) -> Result<impl IntoResponse, ApiError> {
    let actor = authenticate_scoped_request(
        &state,
        &headers,
        Scope::Read,
        "/v1/groups/{name}/memory/{kind}",
    )?
    .principal;
    let memory = state
        .server
        .groups()
        .memory(&name, &kind, &actor.nick)
        .map_err(group_store_error)?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({"ok": true, "memory": memory})),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteGroupMemoryReq {
    text: String,
    #[serde(default)]
    append: bool,
    expected_revision: Option<i64>,
}

async fn write_group_memory(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((name, kind)): Path<(String, String)>,
    Json(req): Json<WriteGroupMemoryReq>,
) -> Result<impl IntoResponse, ApiError> {
    let actor = authenticate_scoped_request(
        &state,
        &headers,
        Scope::Write,
        "/v1/groups/{name}/memory/{kind}",
    )?
    .principal;
    let memory = state
        .server
        .groups()
        .write_memory(
            &name,
            &kind,
            &actor.nick,
            &req.text,
            req.append,
            req.expected_revision,
        )
        .map_err(group_store_error)?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({"ok": true, "memory": memory})),
    ))
}

async fn group_heartbeat(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let actor = authenticate_scoped_request(
        &state,
        &headers,
        Scope::Write,
        "/v1/groups/{name}/heartbeat",
    )?
    .principal;
    let last_activity_at = state
        .server
        .groups()
        .heartbeat(&name, &actor.nick, &actor.id)
        .map_err(group_store_error)?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(
            json!({"ok": true, "group": name, "principal_id": actor.id, "last_activity_at": last_activity_at}),
        ),
    ))
}

async fn group_reassignment_events(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let actor = authenticate_scoped_request(
        &state,
        &headers,
        Scope::Read,
        "/v1/groups/{name}/reassignments",
    )?
    .principal;
    let events = state
        .server
        .groups()
        .reassignment_events(&name, &actor.nick)
        .map_err(group_store_error)?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({"ok": true, "events": events})),
    ))
}

async fn list_groups(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    authenticate_scoped_request(&state, &headers, Scope::Read, "/v1/groups")?;
    let groups = state.server.groups().list().map_err(ApiError::internal)?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({"ok": true, "groups": groups})),
    ))
}

#[derive(Deserialize)]
struct UpsertGroupReq {
    invite: Option<String>,
    #[serde(flatten)]
    group: GroupInput,
}

async fn upsert_group(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<UpsertGroupReq>,
) -> Result<impl IntoResponse, ApiError> {
    require_owner(&state, &headers, req.invite.as_deref())?;
    let group = state
        .server
        .groups()
        .upsert(req.group)
        .map_err(group_store_error)?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({"ok": true, "group": group})),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateGroupMembershipReq {
    invite: Option<String>,
    lead: Option<GroupMember>,
    #[serde(default)]
    add_workers: Vec<GroupMember>,
    #[serde(default)]
    remove_workers: Vec<GroupMember>,
}

async fn update_group_membership(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(req): Json<UpdateGroupMembershipReq>,
) -> Result<impl IntoResponse, ApiError> {
    require_owner(&state, &headers, req.invite.as_deref())?;
    let group = state
        .server
        .groups()
        .update_membership(
            &name,
            GroupMembershipUpdate {
                lead: req.lead,
                add_workers: req.add_workers,
                remove_workers: req.remove_workers,
            },
        )
        .map_err(group_store_error)?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({"ok": true, "group": group})),
    ))
}

#[derive(Deserialize)]
struct AttachGroupReq {
    channel: String,
    invite: Option<String>,
}

async fn attach_group(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(req): Json<AttachGroupReq>,
) -> Result<impl IntoResponse, ApiError> {
    require_owner(&state, &headers, req.invite.as_deref())?;
    let group = state
        .server
        .groups()
        .attach(&name, &req.channel)
        .map_err(group_store_error)?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({"ok": true, "group": group})),
    ))
}

/// Availability is checked when responding, never persisted or probed by launch.
#[derive(Serialize)]
struct TemplateResponse {
    #[serde(flatten)]
    template: ClientTemplate,
    installed: bool,
}

impl From<ClientTemplate> for TemplateResponse {
    fn from(template: ClientTemplate) -> Self {
        let installed = crate::wake::command_found(&template.command);
        Self {
            template,
            installed,
        }
    }
}

fn template_store_error(error: anyhow::Error) -> ApiError {
    if error.downcast_ref::<rusqlite::Error>().is_some() {
        ApiError::internal(error)
    } else {
        ApiError::bad_request(error.to_string())
    }
}

async fn list_templates(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    authenticate_scoped_request(&state, &headers, Scope::Read, "/v1/templates")?;
    let templates: Vec<TemplateResponse> = state
        .server
        .templates()
        .list()
        .map_err(ApiError::internal)?
        .into_iter()
        .map(TemplateResponse::from)
        .collect();
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({ "ok": true, "templates": templates })),
    ))
}

async fn get_template(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    authenticate_scoped_request(&state, &headers, Scope::Read, "/v1/templates")?;
    let template = state
        .server
        .templates()
        .get(&id)
        .map_err(template_store_error)?
        .ok_or_else(|| ApiError::not_found(format!("no client template named '{id}'")))?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({ "ok": true, "template": TemplateResponse::from(template) })),
    ))
}

#[derive(Deserialize)]
struct UpsertTemplateReq {
    invite: Option<String>,
    #[serde(flatten)]
    template: ClientTemplateInput,
}

/// Like launcher configuration, defining commands requires the owner's key.
async fn upsert_template(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<UpsertTemplateReq>,
) -> Result<impl IntoResponse, ApiError> {
    require_owner(&state, &headers, req.invite.as_deref())?;
    let template = state
        .server
        .templates()
        .upsert(&id, req.template)
        .map_err(template_store_error)?;
    state.server.logger().log_security(
        Severity::Info,
        "template.upserted",
        "success",
        Some("owner"),
        Some(&template.id),
        Some("/v1/templates"),
        format!("Owner upserted client template '{}'", template.id),
    );
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({ "ok": true, "template": TemplateResponse::from(template) })),
    ))
}

async fn delete_template(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    require_owner(&state, &headers, None)?;
    if !state
        .server
        .templates()
        .delete(&id)
        .map_err(template_store_error)?
    {
        return Err(ApiError::not_found(format!(
            "no client template named '{id}'"
        )));
    }
    state.server.logger().log_security(
        Severity::Info,
        "template.deleted",
        "success",
        Some("owner"),
        Some(&id),
        Some("/v1/templates"),
        format!("Owner deleted client template '{id}'"),
    );
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({ "ok": true, "deleted": true })),
    ))
}

#[derive(Deserialize)]
struct SpawnTemplateReq {
    invite: Option<String>,
    nick: String,
    model: String,
    channels: Option<Vec<String>>,
    /// Project role; never grants a global owner role.
    role: Option<String>,
}

async fn spawn_template_worker(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<SpawnTemplateReq>,
) -> Result<impl IntoResponse, ApiError> {
    // An explicit invite must be valid; an invalid invite cannot silently
    // fall back to a bearer token. Owner-key calls need no bearer token.
    let actor = if headers.contains_key("x-aircd-invite") || req.invite.is_some() {
        require_owner(&state, &headers, req.invite.as_deref())?;
        None
    } else {
        Some(authenticate_scoped_request(&state, &headers, Scope::Pm, "/v1/templates/spawn")?.principal)
    };
    let created = state
        .server
        .spawn_worker_from_template(
            actor.as_ref(),
            &id,
            &req.model,
            &req.nick,
            req.channels.as_deref().unwrap_or_default(),
            req.role.as_deref(),
        )
        .map_err(|err| {
            if let Some(error) = err.downcast_ref::<SpawnError>() {
                match error {
                    SpawnError::Forbidden(message) => {
                        state.server.logger().log_security(
                            Severity::Warning,
                            "worker.spawn_denied",
                            "failure",
                            actor.as_ref().map(|a| a.id.as_str()),
                            Some(&req.nick),
                            Some("/v1/templates/spawn"),
                            message.clone(),
                        );
                        ApiError::new(StatusCode::FORBIDDEN, message.clone())
                    }
                    SpawnError::TemplateNotFound(message) => ApiError::not_found(message.clone()),
                }
            } else if err.downcast_ref::<DuplicatePrincipalError>().is_some() {
                ApiError::conflict(err.to_string())
            } else if err.downcast_ref::<rusqlite::Error>().is_some() {
                ApiError::internal(err)
            } else {
                ApiError::bad_request(err.to_string())
            }
        })?;
    state.server.logger().log_security(
        Severity::Info,
        "worker.spawned",
        "success",
        actor.as_ref().map(|a| a.id.as_str()).or(Some("owner")),
        Some(&created.principal.id),
        Some(&created.channel),
        format!(
            "Spawned worker @{} from template '{}' (model '{}')",
            created.principal.nick, created.template, created.model
        ),
    );
    let host = resolve_request_host(&headers, &state.info);
    Ok((
        StatusCode::CREATED,
        with_cors_headers(),
        Json(json!({
            "ok": true,
            "account": created,
            "http_url": format!("http://{}:{}", host, state.info.http_port),
        })),
    ))
}

#[derive(Deserialize)]
struct OwnerModelsReq {
    invite: Option<String>,
    template: String,
}

/// Owner-only: the models a template offers. The template's own list comes
/// first, then whatever its listing command prints. The command runs on the
/// hub's machine, which is why this needs the owner key.
async fn owner_models(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<OwnerModelsReq>,
) -> Result<impl IntoResponse, ApiError> {
    require_owner(&state, &headers, req.invite.as_deref())?;
    let template = state
        .server
        .templates()
        .get(&req.template)
        .map_err(template_store_error)?
        .ok_or_else(|| ApiError::not_found(format!("no client template named '{}'", req.template.trim())))?;
    let mut models: Vec<crate::wake::ModelChoice> = template
        .models
        .iter()
        .map(|id| crate::wake::ModelChoice { id: id.clone(), label: None })
        .collect();
    let mut warning = None;
    if !template.models_command.is_empty() {
        match crate::wake::list_models(&template.models_command).await {
            Ok(listed) => {
                for choice in listed {
                    if !models.iter().any(|m| m.id == choice.id) {
                        models.push(choice);
                    }
                }
            }
            Err(err) => warning = Some(err.to_string()),
        }
    }
    if models.is_empty() {
        return Err(ApiError::bad_request(warning.unwrap_or_else(|| {
            format!("template '{}' lists no models; add some to its models list", template.id)
        })));
    }
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({ "ok": true, "template": template.id, "models": models, "warning": warning })),
    ))
}

#[derive(Deserialize)]
struct OwnerCreateAccountReq {
    invite: Option<String>,
    template: String,
    model: String,
    /// Defaults to the model name.
    nick: Option<String>,
    channel: String,
    role: Option<String>,
}

/// Owner-only: create an account that runs one model through a client
/// template and add it to a project. The response carries the account's
/// token, once, for the caller to save as an `airc` profile.
async fn owner_create_account(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<OwnerCreateAccountReq>,
) -> Result<impl IntoResponse, ApiError> {
    require_owner(&state, &headers, req.invite.as_deref())?;
    let created = state
        .server
        .create_account_from_template(
            &req.template,
            &req.model,
            req.nick.as_deref(),
            &req.channel,
            req.role.as_deref(),
        )
        .map_err(|err| {
            if err.downcast_ref::<DuplicatePrincipalError>().is_some() {
                ApiError::conflict(err.to_string())
            } else {
                ApiError::bad_request(err.to_string())
            }
        })?;
    state.server.logger().log_security(
        Severity::Info,
        "account.created",
        "success",
        Some("owner"),
        Some(&created.principal.id),
        Some(&created.channel),
        format!(
            "Owner created account @{} from template '{}' (model '{}')",
            created.principal.nick, created.template, created.model
        ),
    );
    let host = resolve_request_host(&headers, &state.info);
    Ok((
        StatusCode::CREATED,
        with_cors_headers(),
        Json(json!({
            "ok": true,
            "account": created,
            "http_url": format!("http://{}:{}", host, state.info.http_port),
        })),
    ))
}

#[derive(Deserialize)]
struct OwnerWakeReq {
    invite: Option<String>,
    /// `set` (default), `show`, `list`, `enable`, `disable`, `delete`,
    /// `runs`, `test` or `reset`.
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

    if action == "list" {
        let launchers: Vec<serde_json::Value> = wakes
            .list()
            .map_err(ApiError::internal)?
            .into_iter()
            .map(|cfg| {
                let found = crate::wake::command_found(&cfg.command);
                let mut entry = serde_json::to_value(&cfg).unwrap_or(serde_json::Value::Null);
                entry["command_found"] = json!(found);
                entry
            })
            .collect();
        return Ok((
            StatusCode::OK,
            with_cors_headers(),
            Json(json!({ "ok": true, "launchers": launchers })),
        ));
    }

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
            state.server.logger().log_security(
                Severity::Info,
                "wake.config_changed",
                "success",
                Some("owner"),
                Some(&principal.id),
                Some("/v1/owner/wake"),
                format!("Owner updated wake launcher for @{}", principal.nick),
            );
            json!({ "ok": true, "wake": cfg })
        }
        "show" => {
            let cfg = wakes.get(&principal.id).map_err(ApiError::internal)?;
            json!({ "ok": true, "wake": cfg })
        }
        "delete" => {
            let removed = wakes.delete(&principal.id).map_err(ApiError::internal)?;
            if removed {
                state.server.logger().log_security(
                    Severity::Info,
                    "wake.deleted",
                    "success",
                    Some("owner"),
                    Some(&principal.id),
                    Some("/v1/owner/wake"),
                    format!("Owner deleted wake launcher for @{}", principal.nick),
                );
            }
            json!({ "ok": true, "removed": removed })
        }
        "runs" => {
            let runs = wakes
                .runs(Some(&principal.id), req.limit.unwrap_or(20))
                .map_err(ApiError::internal)?;
            json!({ "ok": true, "runs": runs })
        }
        "enable" | "disable" => {
            let enabled = action == "enable";
            let found = wakes
                .set_enabled(&principal.id, enabled)
                .map_err(ApiError::internal)?;
            if !found {
                return Err(ApiError::not_found(format!(
                    "@{} has no launcher configured",
                    principal.nick
                )));
            }
            state.server.logger().log_security(
                Severity::Info,
                "wake.config_changed",
                "success",
                Some("owner"),
                Some(&principal.id),
                Some("/v1/owner/wake"),
                format!("Owner set wake launcher enabled={enabled} for @{}", principal.nick),
            );
            json!({ "ok": true, "nick": principal.nick, "enabled": enabled })
        }
        "reset" => {
            let removed = wakes
                .clear_sessions(&principal.id, None)
                .map_err(ApiError::internal)?;
            state.server.logger().log_security(
                Severity::Info,
                "wake.config_changed",
                "success",
                Some("owner"),
                Some(&principal.id),
                Some("/v1/owner/wake"),
                format!("Owner reset wake sessions for @{}", principal.nick),
            );
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
                "unknown action '{other}': use set, show, list, enable, disable, delete, runs, test or reset"
            )))
        }
    };
    Ok((StatusCode::OK, with_cors_headers(), Json(body)))
}

#[derive(Deserialize)]
struct OwnerReissueReq {
    nick: String,
    invite: Option<String>,
    scopes: Option<Vec<String>>,
    ttl_seconds: Option<u64>,
}

/// Owner-only: revoke all of an agent's tokens and hand back a fresh one.
/// Authorised by the same owner key that guards registration.
async fn owner_reissue_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<OwnerReissueReq>,
) -> Result<impl IntoResponse, ApiError> {
    let Some(expected) = state.info.register_key.as_deref() else {
        state.server.logger().log_security(
            Severity::Warning,
            "owner.auth_denied",
            "failure",
            None,
            Some(&req.nick),
            Some("/v1/owner/reissue"),
            "Token reissue denied: no owner key is configured",
        );
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
        state.server.logger().log_security(
            Severity::Warning,
            "owner.auth_denied",
            "failure",
            None,
            Some(&req.nick),
            Some("/v1/owner/reissue"),
            format!("Token reissue denied for '{}': invalid owner key", req.nick),
        );
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
    let issued = state
        .server
        .reissue_token_with_options(&principal.id, req.scopes.as_deref(), req.ttl_seconds)
        .map_err(|err| {
            if err.downcast_ref::<rusqlite::Error>().is_some() {
                ApiError::internal(err)
            } else {
                ApiError::bad_request(err.to_string())
            }
        })?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({
            "ok": true,
            "principal": principal,
            "token": issued.token,
            "token_id": issued.token_id,
            "scopes": issued.scopes,
            "ttl_seconds": issued.ttl_seconds,
            "expires_at": issued.expires_at,
        })),
    ))
}

#[derive(Deserialize, Default)]
struct OwnerBackupReq {
    invite: Option<String>,
    /// `run` (default), `verify`, or `list`.
    action: Option<String>,
    snapshot_id: Option<String>,
}

async fn owner_list_backups(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    require_owner(&state, &headers, None)?;
    let backups = state.server.backups();
    let status = backups.status();
    let snapshots = backups.list_snapshots().map_err(ApiError::internal)?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({
            "ok": true,
            "status": status,
            "snapshots": snapshots,
        })),
    ))
}

async fn owner_backup_action(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<OwnerBackupReq>>,
) -> Result<impl IntoResponse, ApiError> {
    let req = body.map(|b| b.0).unwrap_or_default();
    require_owner(&state, &headers, req.invite.as_deref())?;
    let action = req
        .action
        .as_deref()
        .unwrap_or("run")
        .trim()
        .to_ascii_lowercase();
    let backups = state.server.backups();
    match action.as_str() {
        "run" => {
            let result = backups
                .run_once("manual")
                .await
                .map_err(|err| ApiError::bad_request(err.to_string()))?;
            let status = backups.status();
            Ok((
                StatusCode::OK,
                with_cors_headers(),
                Json(json!({
                    "ok": true,
                    "result": result,
                    "status": status,
                })),
            ))
        }
        "verify" => {
            let id = req.snapshot_id.clone();
            let manager = backups.clone();
            let verification = tokio::task::spawn_blocking(move || {
                manager.verify_snapshot_by_id(id.as_deref())
            })
            .await
            .map_err(|err| ApiError::internal(anyhow::anyhow!(err)))?
            .map_err(|err| ApiError::bad_request(err.to_string()))?;
            Ok((
                StatusCode::OK,
                with_cors_headers(),
                Json(json!({
                    "ok": true,
                    "verification": verification,
                })),
            ))
        }
        "list" => {
            let status = backups.status();
            let snapshots = backups.list_snapshots().map_err(ApiError::internal)?;
            Ok((
                StatusCode::OK,
                with_cors_headers(),
                Json(json!({
                    "ok": true,
                    "status": status,
                    "snapshots": snapshots,
                })),
            ))
        }
        other => Err(ApiError::bad_request(format!(
            "unsupported backup action '{other}': expected run, verify, or list"
        ))),
    }
}

#[derive(Deserialize, Default)]
struct OwnerDbCheckpointReq {
    invite: Option<String>,
    mode: Option<String>,
}

async fn owner_db_status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    require_owner(&state, &headers, None)?;
    let db = state.server.db().clone();
    let diagnostics = tokio::task::spawn_blocking(move || db.diagnostics())
        .await
        .map_err(|err| ApiError::internal(anyhow::anyhow!(err)))?
        .map_err(ApiError::internal)?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({
            "ok": true,
            "diagnostics": diagnostics,
        })),
    ))
}

async fn owner_db_checkpoint(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<OwnerDbCheckpointReq>>,
) -> Result<impl IntoResponse, ApiError> {
    let req = body.map(|b| b.0).unwrap_or_default();
    require_owner(&state, &headers, req.invite.as_deref())?;
    let mode = WalCheckpointMode::parse(req.mode.as_deref().unwrap_or("passive"))
        .map_err(|err| ApiError::bad_request(err.to_string()))?;
    let db = state.server.db().clone();
    let (checkpoint, diagnostics) = tokio::task::spawn_blocking(move || {
        let checkpoint = db.checkpoint(mode)?;
        let diagnostics = db.diagnostics()?;
        anyhow::Ok((checkpoint, diagnostics))
    })
    .await
    .map_err(|err| ApiError::internal(anyhow::anyhow!(err)))?
    .map_err(ApiError::internal)?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({
            "ok": true,
            "checkpoint": checkpoint,
            "diagnostics": diagnostics,
        })),
    ))
}

#[derive(Deserialize, Default)]
struct LoginReq {
    channels: Option<Vec<String>>,
    rotate_token: Option<bool>,
    label: Option<String>,
    ttl_seconds: Option<u64>,
}

async fn login_agent(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<LoginReq>>,
) -> Result<impl IntoResponse, ApiError> {
    let raw_token = extract_bearer_token(&headers).ok_or_else(|| {
        state.server.logger().log_security(
            Severity::Warning,
            "auth.http_failed",
            "failure",
            None,
            None,
            Some("/v1/login"),
            "HTTP authentication failed: missing bearer token",
        );
        ApiError::unauthorized(
            "Missing Authorization: Bearer <token> header; register via POST /v1/register first",
        )
    })?;
    let ctx = authenticate_token_context(&state, raw_token)?;
    let ctx_for_refresh = ctx.clone();
    let principal = ctx.principal;
    let req = body.map(|b| b.0).unwrap_or_default();
    if req.ttl_seconds == Some(0) {
        return Err(ApiError::bad_request("ttl_seconds must be at least 1"));
    }
    let channels = req.channels.unwrap_or_else(|| vec!["#general".to_string()]);
    let joined = state
        .server
        .join_channels(&principal, &channels)
        .map_err(ApiError::internal)?;

    let (rotated, token_id, scopes, expires_at) = if req.rotate_token.unwrap_or(false) {
        let issued = state
            .server
            .auth()
            .issue_token_with_scopes(
                &principal.id,
                req.label.as_deref(),
                false,
                &ctx.token.scopes,
                req.ttl_seconds.or(ctx.token.ttl_seconds),
            )
            .map_err(|err| {
                if err.downcast_ref::<DuplicateTokenLabelError>().is_some() {
                    ApiError::conflict(err.to_string())
                } else {
                    ApiError::internal(err)
                }
            })?;
        state.server.logger().log_security(
            Severity::Info,
            "token.rotated",
            "success",
            Some(&principal.id),
            Some(&principal.id),
            Some("/v1/login"),
            format!("Issued rotated token for @{}", principal.nick),
        );
        (
            Some(issued.token),
            Some(issued.token_id),
            issued.scopes,
            issued.expires_at,
        )
    } else {
        let (_, refreshed) = state
            .server
            .auth()
            .refresh_token_with_context(ctx_for_refresh, raw_token, req.ttl_seconds, false)
            .map_err(ApiError::internal)?;
        (
            None,
            ctx.token.token_id,
            refreshed.scopes,
            refreshed.expires_at,
        )
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
            "token_id": token_id,
            "scopes": scopes,
            "expires_at": expires_at,
        })),
    ))
}

#[derive(Deserialize)]
struct IssueTokenReq {
    invite: Option<String>,
    nick: Option<String>,
    label: Option<String>,
    scopes: Option<Vec<String>>,
    ttl_seconds: Option<u64>,
    revoke_existing: Option<bool>,
}

async fn issue_token_api(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<IssueTokenReq>,
) -> Result<impl IntoResponse, ApiError> {
    let actor = if headers.contains_key("x-aircd-invite") || req.invite.is_some() {
        require_owner(&state, &headers, req.invite.as_deref())?;
        None
    } else {
        Some(authenticate_scoped_request(
            &state,
            &headers,
            Scope::Admin,
            "/v1/tokens",
        )?)
    };

    let target = match req.nick.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(nick) => state
            .server
            .auth()
            .resolve_by_name(nick)
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::not_found(format!("no agent named '{nick}'")))?,
        None => actor
            .as_ref()
            .map(|ctx| ctx.principal.clone())
            .ok_or_else(|| ApiError::bad_request("nick is required when authenticating with the owner key"))?,
    };

    let issued = state
        .server
        .auth()
        .issue_token_with_options(
            &target.id,
            req.label.as_deref(),
            req.revoke_existing.unwrap_or(false),
            req.scopes.as_deref(),
            req.ttl_seconds,
        )
        .map_err(|err| {
            if err.downcast_ref::<DuplicateTokenLabelError>().is_some() {
                ApiError::conflict(err.to_string())
            } else if err.downcast_ref::<rusqlite::Error>().is_some() {
                ApiError::internal(err)
            } else {
                ApiError::bad_request(err.to_string())
            }
        })?;

    state.server.logger().log_security(
        Severity::Info,
        "token.issued",
        "success",
        actor.as_ref().map(|a| a.principal.id.as_str()).or(Some("owner")),
        Some(&target.id),
        Some("/v1/tokens"),
        format!(
            "Issued token '{}' for @{} (scopes={:?}, expires_at={:?})",
            issued.token_id, target.nick, issued.scopes, issued.expires_at
        ),
    );

    Ok((
        StatusCode::CREATED,
        with_cors_headers(),
        Json(json!({
            "ok": true,
            "principal": target,
            "token": issued.token,
            "token_id": issued.token_id,
            "label": issued.label,
            "scopes": issued.scopes,
            "ttl_seconds": issued.ttl_seconds,
            "expires_at": issued.expires_at,
        })),
    ))
}

#[derive(Deserialize)]
struct TokensQuery {
    nick: Option<String>,
}

async fn list_tokens_api(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<TokensQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let ctx = authenticate_scoped_request(&state, &headers, Scope::Read, "/v1/tokens")?;
    let target = match q.nick.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        Some(nick)
            if !nick
                .trim_start_matches('@')
                .eq_ignore_ascii_case(&ctx.principal.id)
                && !nick
                    .trim_start_matches('@')
                    .eq_ignore_ascii_case(&ctx.principal.nick) =>
        {
            if !ctx.token.has_scope(Scope::Admin) {
                state.server.logger().log_security(
                    Severity::Warning,
                    "auth.scope_denied",
                    "failure",
                    Some(&ctx.principal.id),
                    Some(nick),
                    Some("/v1/tokens"),
                    format!(
                        "HTTP GET /v1/tokens?nick={nick} denied for @{}: token lacks required scope 'admin'",
                        ctx.principal.nick
                    ),
                );
                return Err(ApiError::forbidden(
                    "token lacks required scope 'admin' to list another account's tokens",
                ));
            }
            state
                .server
                .auth()
                .resolve_by_name(nick)
                .map_err(ApiError::internal)?
                .ok_or_else(|| ApiError::not_found(format!("no agent named '{nick}'")))?
        }
        _ => ctx.principal,
    };

    let tokens = state
        .server
        .auth()
        .list_tokens(&target.id)
        .map_err(ApiError::internal)?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({
            "ok": true,
            "principal": target,
            "tokens": tokens,
        })),
    ))
}

#[derive(Deserialize, Default)]
struct RefreshTokenReq {
    ttl_seconds: Option<u64>,
    rotate: Option<bool>,
}

async fn refresh_token_api(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<RefreshTokenReq>>,
) -> Result<impl IntoResponse, ApiError> {
    let raw_token = extract_bearer_token(&headers).ok_or_else(|| {
        state.server.logger().log_security(
            Severity::Warning,
            "auth.http_failed",
            "failure",
            None,
            None,
            Some("/v1/tokens/refresh"),
            "HTTP authentication failed: missing bearer token",
        );
        ApiError::unauthorized(
            "Missing Authorization: Bearer <token> header; register via POST /v1/register first",
        )
    })?;
    let req = body.map(|b| b.0).unwrap_or_default();
    if req.ttl_seconds == Some(0) {
        return Err(ApiError::bad_request("ttl_seconds must be at least 1"));
    }
    let rotate = req.rotate.unwrap_or(false);
    let ctx = authenticate_token_context(&state, raw_token)?;
    let (principal, refreshed) = state
        .server
        .auth()
        .refresh_token_with_context(ctx, raw_token, req.ttl_seconds, rotate)
        .map_err(|err| ApiError::bad_request(err.to_string()))?;

    state.server.logger().log_security(
        Severity::Info,
        "token.refreshed",
        "success",
        Some(&principal.id),
        Some(&principal.id),
        Some("/v1/tokens/refresh"),
        format!(
            "Refreshed token '{}' for @{} (rotate={}, expires_at={:?})",
            refreshed.token_id, principal.nick, rotate, refreshed.expires_at
        ),
    );

    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({
            "ok": true,
            "principal": principal,
            "token": refreshed.token,
            "token_id": refreshed.token_id,
            "label": refreshed.label,
            "scopes": refreshed.scopes,
            "ttl_seconds": refreshed.ttl_seconds,
            "expires_at": refreshed.expires_at,
        })),
    ))
}

async fn revoke_token_api(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(token_id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let actor = if headers.contains_key("x-aircd-invite") {
        require_owner(&state, &headers, None)?;
        None
    } else {
        Some(authenticate_scoped_request(
            &state,
            &headers,
            Scope::Admin,
            "/v1/tokens/{token_id}",
        )?)
    };
    let revoked = state
        .server
        .auth()
        .revoke_token(&token_id)
        .map_err(ApiError::internal)?;
    if !revoked {
        return Err(ApiError::not_found(format!(
            "no active token with id '{token_id}'"
        )));
    }
    state.server.logger().log_security(
        Severity::Info,
        "token.revoked",
        "success",
        actor.as_ref().map(|a| a.principal.id.as_str()).or(Some("owner")),
        Some(&token_id),
        Some("/v1/tokens"),
        format!("Revoked token '{token_id}'"),
    );
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({ "ok": true, "revoked": true, "token_id": token_id })),
    ))
}

async fn get_me(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    let ctx = authenticate_scoped_request(&state, &headers, Scope::Read, "/v1/me")?;
    let webhook = state
        .server
        .webhooks()
        .get(&ctx.principal.id)
        .map_err(ApiError::internal)?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({
            "ok": true,
            "principal": ctx.principal,
            "webhook": webhook,
            "token": ctx.token,
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
    let principal =
        authenticate_scoped_request(&state, &headers, Scope::Write, "/v1/channels/join")?
            .principal;
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
    let principal =
        authenticate_scoped_request(&state, &headers, Scope::Read, "/v1/messages")?.principal;
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
    let principal =
        authenticate_scoped_request(&state, &headers, Scope::Write, "/v1/messages")?.principal;
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
    let _ = authenticate_scoped_request(&state, &headers, Scope::Read, "/v1/roles")?;
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
    let actor =
        authenticate_scoped_request(&state, &headers, Scope::Admin, "/v1/roles")?.principal;
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
    let principal = authenticate_scoped_request(&state, &headers, Scope::Read, "/v1/webhooks")?
        .principal;
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
    let principal = authenticate_scoped_request(&state, &headers, Scope::Write, "/v1/webhooks")?
        .principal;
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

    state.server.logger().log_security(
        Severity::Info,
        "webhook.updated",
        "success",
        Some(&principal.id),
        Some(&principal.id),
        Some("/v1/webhooks"),
        format!(
            "Webhook updated for @{} (rotate_secret={})",
            principal.nick,
            req.rotate_secret.unwrap_or(false)
        ),
    );

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
    let principal = authenticate_scoped_request(&state, &headers, Scope::Write, "/v1/webhooks")?
        .principal;
    let deleted = state
        .server
        .webhooks()
        .delete(&principal.id)
        .map_err(ApiError::internal)?;
    if deleted {
        state.server.logger().log_security(
            Severity::Info,
            "webhook.deleted",
            "success",
            Some(&principal.id),
            Some(&principal.id),
            Some("/v1/webhooks"),
            format!("Webhook deleted for @{}", principal.nick),
        );
    }
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
    let principal =
        authenticate_scoped_request(&state, &headers, Scope::Write, "/v1/webhooks/test")?
            .principal;
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
    let principal = authenticate_scoped_request(
        &state,
        &headers,
        Scope::Read,
        "/v1/webhooks/deliveries",
    )?
    .principal;
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
    let principal = authenticate_scoped_request(&state, &headers, Scope::Write, "/v1/reports")?
        .principal;
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
    let _ = authenticate_scoped_request(&state, &headers, Scope::Read, "/v1/reports")?;
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
    let _ = authenticate_scoped_request(&state, &headers, Scope::Read, "/v1/reports/{id}")?;
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
    let _ = authenticate_scoped_request(&state, &headers, Scope::Read, "/v1/tasks")?;
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
    reviewer: Option<String>,
    parent_task_id: Option<String>,
}

async fn create_task(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CreateTaskReq>,
) -> Result<impl IntoResponse, ApiError> {
    let actor = authenticate_scoped_request(&state, &headers, Scope::Pm, "/v1/tasks")?.principal;
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
                sprint: None,
                reviewer: req.reviewer,
                parent_task_id: req.parent_task_id,
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
    let actor =
        authenticate_scoped_request(&state, &headers, Scope::Pm, "/v1/tasks/{id}/assign")?
            .principal;
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
    report_body: Option<String>,
    report_id: Option<String>,
    commit: Option<String>,
}

async fn set_task_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<TaskStatusReq>,
) -> Result<impl IntoResponse, ApiError> {
    let normalized_status = req.status.trim().to_ascii_lowercase().replace('-', "_");
    let required_scope = match normalized_status.as_str() {
        "in_progress" | "testing" | "review" => Scope::Write,
        _ => Scope::Pm,
    };
    let actor = authenticate_scoped_request(
        &state,
        &headers,
        required_scope,
        "/v1/tasks/{id}/status",
    )?
    .principal;
    let task = state
        .server
        .set_task_status_revision_api(
            &actor,
            &id,
            &req.status,
            req.note.as_deref(),
            req.branch.as_deref(),
            req.report_body.as_deref(),
            req.report_id.as_deref(),
            req.commit.as_deref(),
        )
        .map_err(|err| ApiError::bad_request(err.to_string()))?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({ "ok": true, "task": task })),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PeerReviewTaskReq {
    revision: String,
    decision: String,
    note: String,
}

async fn peer_review_task(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<PeerReviewTaskReq>,
) -> Result<impl IntoResponse, ApiError> {
    let actor = authenticate_scoped_request(
        &state,
        &headers,
        Scope::Write,
        "/v1/tasks/{id}/peer-review",
    )?
    .principal;
    let task = state
        .server
        .peer_review_task_api(&actor, &id, &req.revision, &req.decision, &req.note)
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({"ok": true, "task": task})),
    ))
}

async fn get_task(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let _ = authenticate_scoped_request(&state, &headers, Scope::Read, "/v1/tasks/{id}")?;
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
    let subtasks = state
        .server
        .tasks()
        .children(&task.id)
        .map_err(ApiError::internal)?;
    let attachments = state
        .server
        .attachments()
        .list(&task.id)
        .map_err(ApiError::internal)?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({
            "ok": true,
            "task": task,
            "events": events,
            "question": question,
            "subtasks": subtasks,
            "attachments": attachments
        })),
    ))
}

#[derive(Deserialize)]
struct AttachQuery {
    name: Option<String>,
    filename: Option<String>,
    kind: Option<String>,
    note: Option<String>,
}

/// Upload one file as the raw request body. The name, kind (`log` or
/// `evidence`) and an optional note travel in the query string.
async fn upload_task_attachment(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<AttachQuery>,
    body: Bytes,
) -> Result<impl IntoResponse, ApiError> {
    let actor = authenticate_scoped_request(
        &state,
        &headers,
        Scope::Write,
        "/v1/tasks/{id}/attachments",
    )?
    .principal;
    let task = state
        .server
        .get_task_api(&id)
        .map_err(|err| ApiError::not_found(err.to_string()))?;
    let filename = q
        .name
        .or(q.filename)
        .ok_or_else(|| ApiError::bad_request("name is required (?name=<file name>)"))?;
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());
    let attachment = state
        .server
        .attach_task_file_api(
            &actor,
            &task.id,
            &filename,
            content_type,
            q.kind.as_deref(),
            q.note.as_deref(),
            &body,
        )
        .map_err(|err| {
            let message = err.to_string();
            if message.contains("can attach files") {
                ApiError::new(StatusCode::FORBIDDEN, message)
            } else {
                ApiError::bad_request(message)
            }
        })?;
    Ok((
        StatusCode::CREATED,
        with_cors_headers(),
        Json(json!({ "ok": true, "attachment": attachment })),
    ))
}

async fn list_task_attachments(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let _ = authenticate_scoped_request(
        &state,
        &headers,
        Scope::Read,
        "/v1/tasks/{id}/attachments",
    )?;
    let task = state
        .server
        .get_task_api(&id)
        .map_err(|err| ApiError::not_found(err.to_string()))?;
    let attachments = state
        .server
        .attachments()
        .list(&task.id)
        .map_err(ApiError::internal)?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({ "ok": true, "task_id": task.id, "attachments": attachments })),
    ))
}

/// The stored bytes, always as a download so a browser never renders them.
async fn download_task_attachment(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, attachment_id)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let _ = authenticate_scoped_request(
        &state,
        &headers,
        Scope::Read,
        "/v1/tasks/{id}/attachments/{attachment_id}",
    )?;
    let (meta, data) = state
        .server
        .attachments()
        .get(id.trim(), attachment_id.trim())
        .map_err(ApiError::internal)?
        .ok_or_else(|| {
            ApiError::not_found(format!(
                "task '{}' has no attachment '{}'",
                id.trim(),
                attachment_id.trim()
            ))
        })?;
    let disposition = format!("attachment; filename=\"{}\"", meta.filename);
    let mut response = (StatusCode::OK, with_cors_headers(), data).into_response();
    let out = response.headers_mut();
    for (name, value) in [
        (header::CONTENT_TYPE, meta.content_type.as_str()),
        (header::CONTENT_DISPOSITION, disposition.as_str()),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::ETAG, meta.sha256.as_str()),
    ] {
        if let Ok(value) = header::HeaderValue::from_str(value) {
            out.insert(name, value);
        }
    }
    if !out.contains_key(header::CONTENT_DISPOSITION) {
        out.insert(
            header::CONTENT_DISPOSITION,
            header::HeaderValue::from_static("attachment"),
        );
    }
    out.insert(
        header::HeaderName::from_static("x-aircd-sha256"),
        header::HeaderValue::from_str(&meta.sha256).map_err(|e| ApiError::internal(e.into()))?,
    );
    Ok(response)
}

async fn get_inbox(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    let actor =
        authenticate_scoped_request(&state, &headers, Scope::Read, "/v1/inbox")?.principal;
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
    let actor =
        authenticate_scoped_request(&state, &headers, Scope::Admin, "/v1/projects")?.principal;
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

#[derive(Deserialize)]
struct OpenRoomReq {
    project: String,
    title: Option<String>,
}

async fn open_room(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<OpenRoomReq>,
) -> Result<impl IntoResponse, ApiError> {
    let actor = authenticate_scoped_request(&state, &headers, Scope::Pm, "/v1/rooms")?.principal;
    let project = normalize_http_channel(&req.project);
    let room = state
        .server
        .open_room_api(&actor, &project, req.title.as_deref())
        .map_err(|err| ApiError::bad_request(err.to_string()))?;
    Ok((
        StatusCode::CREATED,
        with_cors_headers(),
        Json(json!({ "ok": true, "room": room })),
    ))
}

/// Sprint rooms of one project (`?channel=`), or of every project.
async fn list_rooms(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ChannelQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let _ = authenticate_scoped_request(&state, &headers, Scope::Read, "/v1/rooms")?;
    let project = q.channel.as_deref().map(normalize_http_channel);
    let rooms = state
        .server
        .rooms()
        .list(project.as_deref())
        .map_err(ApiError::internal)?;
    Ok((
        StatusCode::OK,
        with_cors_headers(),
        Json(json!({ "ok": true, "rooms": rooms })),
    ))
}

async fn list_projects(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    let _ = authenticate_scoped_request(&state, &headers, Scope::Read, "/v1/projects")?;
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
    let actor = authenticate_scoped_request(
        &state,
        &headers,
        Scope::Pm,
        "/v1/projects/members",
    )?
    .principal;
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
    let _ = authenticate_scoped_request(&state, &headers, Scope::Read, "/v1/projects/members")?;
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
    let actor = authenticate_scoped_request(&state, &headers, Scope::Write, "/v1/tasks/{id}/ask")?
        .principal;
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
    let actor = authenticate_scoped_request(&state, &headers, Scope::Pm, "/v1/tasks/{id}/answer")?
        .principal;
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
    let actor =
        authenticate_scoped_request(&state, &headers, Scope::Pm, "/v1/tasks/{id}/escalate")?
            .principal;
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
    let actor =
        authenticate_scoped_request(&state, &headers, Scope::Read, "/v1/notes")?.principal;
    let target_id = match q.nick.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        Some(nick) if !nick.trim_start_matches('@').eq_ignore_ascii_case(&actor.id) => {
            if !Server::is_owner(&actor) {
                state.server.logger().log_security(
                    Severity::Warning,
                    "notes.access_denied",
                    "failure",
                    Some(&actor.id),
                    Some(nick),
                    Some("/v1/notes"),
                    format!(
                        "Non-owner @{} attempted to read notes for '{}'",
                        actor.nick, nick
                    ),
                );
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
    let actor =
        authenticate_scoped_request(&state, &headers, Scope::Write, "/v1/notes")?.principal;
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
    let actor =
        authenticate_scoped_request(&state, &headers, Scope::Admin, "/v1/projects/pm")?.principal;
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
    let actor =
        authenticate_scoped_request(&state, &headers, Scope::Write, "/v1/tasks/{id}/claim")?
            .principal;
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
    let actor =
        authenticate_scoped_request(&state, &headers, Scope::Write, "/v1/tasks/{id}/done")?
            .principal;
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
    let actor =
        authenticate_scoped_request(&state, &headers, Scope::Write, "/v1/tasks/{id}/release")?
            .principal;
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
    let actor =
        authenticate_scoped_request(&state, &headers, Scope::Write, "/v1/locks")?.principal;
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
    let required_scope = if req.force.unwrap_or(false) {
        Scope::Admin
    } else {
        Scope::Write
    };
    let actor =
        authenticate_scoped_request(&state, &headers, required_scope, "/v1/locks/release")?
            .principal;
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
    let ctx =
        authenticate_scoped_request(&state, &headers, Scope::Write, "/v1/locks/{resource}")?;
    let actor = ctx.principal;
    let is_privileged = matches!(
        actor.role.as_str(),
        "leader" | "coordinator" | "op" | "admin"
    );
    let can_force = is_privileged && ctx.token.has_scope(Scope::Admin);
    let released = state
        .server
        .release_lock_api(&actor, &resource, can_force)
        .map_err(|err| {
            if err.downcast_ref::<LockConflictError>().is_some() {
                if is_privileged && !ctx.token.has_scope(Scope::Admin) {
                    state.server.logger().log_security(
                        Severity::Warning,
                        "auth.scope_denied",
                        "denied",
                        Some(&actor.id),
                        Some(&resource),
                        Some("/v1/locks/{resource}"),
                        format!(
                            "Scope denied for @{} on /v1/locks/{{resource}}: required=admin, token_scopes={:?}",
                            actor.nick, ctx.token.scopes
                        ),
                    );
                    ApiError::forbidden(
                        "Token lacks required scope 'admin' to force-unlock another agent's resource",
                    )
                } else {
                    ApiError::conflict(err.to_string())
                }
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
    let actor = authenticate_scoped_request(&state, &headers, Scope::Pm, "/v1/gits")?.principal;
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
    let actor =
        authenticate_scoped_request(&state, &headers, Scope::Write, "/v1/git-events")?.principal;
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
        .ok_or_else(|| {
            state.server.logger().log_security(
                Severity::Warning,
                "git.webhook_denied",
                "failure",
                None,
                Some(&clean_repo),
                Some("/v1/git/webhook"),
                format!("GitHub webhook rejected for repo '{clean_repo}': no webhook_secret configured"),
            );
            ApiError::unauthorized("Repo has no webhook_secret configured")
        })?;
    let sig_header = headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if !verify_github_signature(secret, raw_body, sig_header) {
        state.server.logger().log_security(
            Severity::Warning,
            "git.webhook_denied",
            "failure",
            None,
            Some(&clean_repo),
            Some("/v1/git/webhook"),
            format!("GitHub webhook rejected for repo '{clean_repo}': invalid X-Hub-Signature-256"),
        );
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

fn extract_bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
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
}

fn authenticate_token_context(state: &AppState, raw: &str) -> Result<AuthContext, ApiError> {
    if let Some(outcome) = REQUEST_TOKEN_AUTH
        .try_with(|(cached_raw, outcome)| (cached_raw == raw).then(|| outcome.clone()))
        .ok()
        .flatten()
    {
        return match outcome {
            CachedTokenAuth::Ok(ctx) => Ok(ctx),
            CachedTokenAuth::Expired => Err(ApiError::unauthorized("Bearer token has expired")),
            CachedTokenAuth::Invalid => {
                state.server.logger().log_security(
                    Severity::Warning,
                    "auth.http_failed",
                    "failure",
                    None,
                    None,
                    None,
                    "HTTP authentication failed: invalid bearer token",
                );
                Err(ApiError::unauthorized("Invalid bearer token"))
            }
        };
    }
    state
        .server
        .auth()
        .authenticate_context(raw)
        .map_err(|err| {
            if err.downcast_ref::<TokenExpiredError>().is_some() {
                ApiError::unauthorized("Bearer token has expired")
            } else {
                state.server.logger().log_security(
                    Severity::Warning,
                    "auth.http_failed",
                    "failure",
                    None,
                    None,
                    None,
                    "HTTP authentication failed: invalid bearer token",
                );
                ApiError::unauthorized("Invalid bearer token")
            }
        })
}

fn authenticate_context_request(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<AuthContext, ApiError> {
    let raw = extract_bearer_token(headers).ok_or_else(|| {
        state.server.logger().log_security(
            Severity::Warning,
            "auth.http_failed",
            "failure",
            None,
            None,
            None,
            "HTTP authentication failed: missing bearer token",
        );
        ApiError::unauthorized(
            "Missing Authorization: Bearer <token> header; register via POST /v1/register first",
        )
    })?;
    authenticate_token_context(state, raw)
}

fn authenticate_scoped_request(
    state: &AppState,
    headers: &HeaderMap,
    required: Scope,
    endpoint: &str,
) -> Result<AuthContext, ApiError> {
    let ctx = authenticate_context_request(state, headers)?;
    if !ctx.token.has_scope(required) {
        state.server.logger().log_security(
            Severity::Warning,
            "auth.scope_denied",
            "failure",
            Some(&ctx.principal.id),
            None,
            Some(endpoint),
            format!(
                "HTTP {} denied for @{}: token lacks required scope '{}'",
                endpoint,
                ctx.principal.nick,
                required.as_str()
            ),
        );
        return Err(ApiError::forbidden(format!(
            "token lacks required scope '{}' for {endpoint}",
            required.as_str()
        )));
    }
    Ok(ctx)
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
            "http_tls_port": state.info.http_tls_port,
            "tls_port": state.info.tls_port,
            "tls_client_auth": state.info.tls_client_auth,
            "tls_only": state.info.tls_only,
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
    <p class="muted">Web &amp; machine-readable endpoints: <a style="color:var(--accent)" href="/dashboard">Sprint Dashboard</a> &middot; <a style="color:var(--accent)" href="/v1/welcome">/v1/welcome</a> (JSON) &middot; <a style="color:var(--accent)" href="/?format=md">/?format=md</a> (Markdown) &middot; <a style="color:var(--accent)" href="/healthz">/healthz</a> &middot; <a style="color:var(--accent)" href="/metrics">/metrics</a> (Prometheus)</p>
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
            "Authorization, Content-Type, X-Aircd-Token, X-AirCD-Invite",
        ),
        (
            header::ACCESS_CONTROL_ALLOW_METHODS,
            "GET, POST, PATCH, DELETE, OPTIONS",
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

    include!("http_delegation_tests.rs");
    include!("http_consensus_tests.rs");
    include!("http_group_memory_tests.rs");
    include!("http_attachment_tests.rs");
    include!("http_group_activity_tests.rs");
    include!("http_rate_limit_tests.rs");
    include!("http_metrics_tests.rs");
    include!("http_dashboard_tests.rs");

    struct TemplateApi {
        server: Server,
        db: store::SharedConnection,
        client: reqwest::Client,
        base: String,
        token: String,
        task: tokio::task::JoinHandle<()>,
    }

    impl TemplateApi {
        async fn start(owner_key: Option<&str>) -> Result<Self> {
            let db = store::open(":memory:")?;
            let server = Server::new(db.clone());
            let (_, token) = server
                .auth()
                .register_principal("reader", Some("member"), None)?;
            let listener = TcpListener::bind("127.0.0.1:0").await?;
            let addr = listener.local_addr()?;
            let mut info = build_runtime_info("127.0.0.1:6667", &addr.to_string(), None);
            info.register_key = owner_key.map(str::to_string);
            let router = router(server.clone(), info);
            let task = tokio::spawn(async move {
                axum::serve(listener, router).await.unwrap();
            });
            Ok(Self {
                server,
                db,
                client: reqwest::Client::new(),
                base: format!("http://{addr}"),
                token,
                task,
            })
        }

        async fn request(
            &self,
            method: &str,
            path: &str,
            token: Option<&str>,
            key: Option<&str>,
            body: Option<&Value>,
        ) -> Result<(StatusCode, Value)> {
            let mut request = self
                .client
                .request(method.parse()?, format!("{}{path}", self.base));
            if let Some(token) = token {
                request = request.bearer_auth(token);
            }
            if let Some(key) = key {
                request = request.header("X-AirCD-Invite", key);
            }
            if let Some(body) = body {
                request = request.json(body);
            }
            let response = request.send().await?;
            let status = response.status();
            let body = response.json().await?;
            Ok((status, body))
        }

        async fn read(&self, path: &str) -> Result<(StatusCode, Value)> {
            self.request("GET", path, Some(&self.token), None, None)
                .await
        }

        async fn write(
            &self,
            method: &str,
            path: &str,
            body: Option<&Value>,
        ) -> Result<(StatusCode, Value)> {
            self.request(method, path, None, Some("template-owner-key"), body)
                .await
        }
    }

    impl Drop for TemplateApi {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    fn template_body() -> Value {
        json!({"name": "Example Client", "command": "aircd-uninstalled-example-client"})
    }

    fn group_body() -> Value {
        json!({
            "name": "dev",
            "description": "Opus lead with local workers",
            "lead": {"template": "claude", "model": "opus", "count": 1, "nick": "dev-lead"},
            "workers": [{"template": "ollama", "model": "qwen3:35b", "count": 3}],
            "lead_prompt": "Split {task}.\nTest and review.\n",
            "worker_prompt": "Write the assigned code.\n"
        })
    }

    #[tokio::test]
    async fn groups_membership_http_updates_attached_groups_without_provisioning() -> Result<()> {
        let api = spawn_api(Some("template-owner-key")).await?;
        let counts = provisioning_counts(&api)?;
        api.write("POST", "/v1/groups", Some(&group_body())).await?;
        let (_, attached) = api
            .write(
                "POST",
                "/v1/groups/dev/attach",
                Some(&json!({"channel": "#one"})),
            )
            .await?;
        let (status, response) = api
            .write(
                "PATCH",
                "/v1/groups/dev",
                Some(&json!({
                    "lead": {"template": "codex", "model": "default", "nick": "lead2"},
                    "remove_workers": [{"template": "ollama", "model": "qwen3:35b", "count": 2}],
                    "add_workers": [{"template": "claude", "model": "sonnet", "nick": "worker1"}]
                })),
            )
            .await?;
        assert_eq!(status, StatusCode::OK, "{response}");
        let group = &response["group"];
        assert_eq!(group["lead"]["nick"], "lead2");
        assert_eq!(group["workers"][0]["count"], 1);
        assert_eq!(group["workers"][1]["nick"], "worker1");
        for key in [
            "projects",
            "created_at",
            "description",
            "lead_prompt",
            "worker_prompt",
        ] {
            assert_eq!(group[key], attached["group"][key]);
        }
        assert_eq!(api.read("/v1/groups").await?.1["groups"][0], *group);
        assert_eq!(provisioning_counts(&api)?, counts);
        assert!(api.server.wakes().runs(None, 10)?.is_empty());
        let mut named = group_body();
        named["name"] = json!("dev équipe");
        api.write("POST", "/v1/groups", Some(&named)).await?;
        assert_eq!(
            api.write(
                "PATCH",
                "/v1/groups/dev%20%C3%A9quipe",
                Some(&json!({
                    "add_workers": [{"template": "c", "model": "m"}]
                }))
            )
            .await?
            .0,
            StatusCode::OK
        );
        let preflight = api
            .client
            .request(
                reqwest::Method::OPTIONS,
                format!("{}/v1/groups/dev", api.base),
            )
            .header("Origin", "http://example.test")
            .header("Access-Control-Request-Method", "PATCH")
            .send()
            .await?;
        assert_eq!(preflight.status(), StatusCode::NO_CONTENT);
        assert!(preflight.headers()[header::ACCESS_CONTROL_ALLOW_METHODS]
            .to_str()?
            .contains("PATCH"));
        assert!(preflight.headers()[header::ACCESS_CONTROL_ALLOW_HEADERS]
            .to_str()?
            .contains("X-AirCD-Invite"));
        Ok(())
    }

    #[tokio::test]
    async fn groups_membership_http_enforces_owner_key_and_closed_writes() -> Result<()> {
        let api = spawn_api(Some("template-owner-key")).await?;
        api.write("POST", "/v1/groups", Some(&group_body())).await?;
        let before = api.server.groups().list()?;
        let body = json!({"add_workers": [{"template": "c", "model": "m"}]});
        let (_, owner_token) =
            api.server
                .auth()
                .register_principal("owner", Some("leader"), None)?;
        for token in [
            None,
            Some("invalid"),
            Some(api.token.as_str()),
            Some(owner_token.as_str()),
        ] {
            for key in [None, Some("wrong")] {
                assert_eq!(
                    api.request("PATCH", "/v1/groups/dev", token, key, Some(&body))
                        .await?
                        .0,
                    StatusCode::FORBIDDEN
                );
                assert_eq!(api.server.groups().list()?, before);
            }
        }
        let mut with_key = body.clone();
        with_key["invite"] = json!("template-owner-key");
        assert_eq!(
            api.request(
                "PATCH",
                "/v1/groups/dev",
                None,
                Some("wrong"),
                Some(&with_key)
            )
            .await?
            .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(api.server.groups().list()?, before);
        assert_eq!(
            api.request("PATCH", "/v1/groups/dev", None, None, Some(&with_key))
                .await?
                .0,
            StatusCode::OK
        );
        with_key["invite"] = json!("wrong");
        assert_eq!(
            api.write("PATCH", "/v1/groups/dev", Some(&with_key))
                .await?
                .0,
            StatusCode::OK
        );
        let open = TemplateApi::start(None).await?;
        assert_eq!(
            open.write("PATCH", "/v1/groups/dev", Some(&body)).await?.0,
            StatusCode::FORBIDDEN
        );
        assert!(open.server.groups().list()?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn groups_membership_http_rejects_bad_batches_and_storage_errors() -> Result<()> {
        let api = spawn_api(Some("template-owner-key")).await?;
        api.write("POST", "/v1/groups", Some(&group_body())).await?;
        let before = api.server.groups().list()?;
        for (body, expected) in [
            (json!({}), StatusCode::BAD_REQUEST),
            (
                json!({"remove_workers": [{"template": "ollama", "model": "qwen3:35b", "count": 3}]}),
                StatusCode::BAD_REQUEST,
            ),
            (
                json!({"add_workers": [{"template": "c", "model": "m", "count": 0}]}),
                StatusCode::BAD_REQUEST,
            ),
            (
                json!({"lead": {"template": "c", "model": "m", "count": 2}}),
                StatusCode::BAD_REQUEST,
            ),
            (
                json!({"add_workers": [{"template": "c", "model": "m", "count": -1}]}),
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (
                json!({"add_workers": [{"template": "c", "model": "m", "count": 4294967296u64}]}),
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (
                json!({"remove_workers": [{"template": "no", "model": "match"}], "lead": {"template": "c", "model": "m"}}),
                StatusCode::BAD_REQUEST,
            ),
            (json!({"workers": []}), StatusCode::UNPROCESSABLE_ENTITY),
        ] {
            let response = api
                .client
                .patch(format!("{}/v1/groups/dev", api.base))
                .header("X-AirCD-Invite", "template-owner-key")
                .json(&body)
                .send()
                .await?;
            assert_eq!(response.status(), expected, "{body}");
            assert_eq!(api.server.groups().list()?, before);
        }
        assert_eq!(
            api.write(
                "PATCH",
                "/v1/groups/missing",
                Some(&json!({
                    "add_workers": [{"template": "c", "model": "m"}]
                }))
            )
            .await?
            .0,
            StatusCode::NOT_FOUND
        );
        api.db
            .lock()
            .unwrap()
            .execute("UPDATE groups SET workers_json = 'corrupt'", [])?;
        assert_eq!(
            api.write(
                "PATCH",
                "/v1/groups/dev",
                Some(&json!({
                    "add_workers": [{"template": "c", "model": "m"}]
                }))
            )
            .await?
            .0,
            StatusCode::INTERNAL_SERVER_ERROR
        );
        Ok(())
    }

    #[tokio::test]
    async fn groups_http_round_trips_definitions_and_attaches_without_provisioning() -> Result<()> {
        let api = spawn_api(Some("template-owner-key")).await?;
        let counts = provisioning_counts(&api)?;
        assert_eq!(
            api.read("/v1/groups").await?.1,
            json!({"ok": true, "groups": []})
        );
        let input = group_body();
        let (status, body) = api.write("POST", "/v1/groups", Some(&input)).await?;
        assert_eq!(status, StatusCode::OK, "{body}");
        for (key, value) in input.as_object().unwrap() {
            assert_eq!(&body["group"][key], value, "{key}");
        }
        assert_eq!(body["group"]["projects"], json!([]));
        let created_at = body["group"]["created_at"].clone();
        let path = "/v1/groups/dev/attach";
        for channel in ["#two", "#one", "#one"] {
            let (status, response) = api
                .write("POST", path, Some(&json!({"channel": channel})))
                .await?;
            assert_eq!(status, StatusCode::OK, "{response}");
        }
        let attached = api.read("/v1/groups").await?.1["groups"][0].clone();
        assert_eq!(attached["projects"], json!(["#one", "#two"]));
        assert_eq!(attached["created_at"], created_at);
        let mut replacement = input.clone();
        replacement["worker_prompt"] = json!("Replacement");
        replacement.as_object_mut().unwrap().remove("lead_prompt");
        // Clients cannot bypass attach by supplying response metadata on create.
        replacement["projects"] = json!(["#unregistered"]);
        let (status, response) = api.write("POST", "/v1/groups", Some(&replacement)).await?;
        assert_eq!(status, StatusCode::OK, "{response}");
        assert_eq!(response["group"]["lead_prompt"], Value::Null);
        assert_eq!(response["group"]["worker_prompt"], "Replacement");
        assert_eq!(response["group"]["projects"], attached["projects"]);
        assert_eq!(response["group"]["created_at"], created_at);
        // Group names are URL path segments, including encoded spaces and Unicode.
        let mut named = input;
        named["name"] = json!("dev équipe");
        api.write("POST", "/v1/groups", Some(&named)).await?;
        let (status, response) = api
            .write(
                "POST",
                "/v1/groups/dev%20%C3%A9quipe/attach",
                Some(&json!({"channel": "#one"})),
            )
            .await?;
        assert_eq!(status, StatusCode::OK, "{response}");
        assert_eq!(response["group"]["name"], "dev équipe");
        assert_eq!(
            api.read("/v1/groups").await?.1["groups"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(provisioning_counts(&api)?, counts);
        assert!(api.server.wakes().runs(None, 10)?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn groups_http_authenticates_reads_and_guards_all_writes_with_owner_key() -> Result<()> {
        let api = spawn_api(Some("template-owner-key")).await?;
        api.write("POST", "/v1/groups", Some(&group_body())).await?;
        let (_, owner_token) =
            api.server
                .auth()
                .register_principal("owner", Some("leader"), None)?;
        for token in [None, Some("invalid-token")] {
            assert_eq!(
                api.request("GET", "/v1/groups", token, None, None).await?.0,
                StatusCode::UNAUTHORIZED
            );
        }
        assert_eq!(
            api.request("GET", "/v1/groups", None, Some("template-owner-key"), None)
                .await?
                .0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(api.read("/v1/groups").await?.0, StatusCode::OK);
        for (path, body) in [
            ("/v1/groups", group_body()),
            ("/v1/groups/dev/attach", json!({"channel": "#one"})),
        ] {
            let before = api.server.groups().list()?;
            for token in [None, Some(api.token.as_str()), Some(owner_token.as_str())] {
                for key in [None, Some("wrong-key")] {
                    let (status, response) =
                        api.request("POST", path, token, key, Some(&body)).await?;
                    assert_eq!(status, StatusCode::FORBIDDEN, "{path}: {response}");
                    assert_eq!(response["ok"], false);
                    assert_eq!(api.server.groups().list()?, before);
                }
            }
            let mut with_key = body;
            with_key["invite"] = json!("template-owner-key");
            assert_eq!(
                api.request("POST", path, None, Some("wrong-key"), Some(&with_key))
                    .await?
                    .0,
                StatusCode::FORBIDDEN
            );
            assert_eq!(
                api.request("POST", path, None, None, Some(&with_key))
                    .await?
                    .0,
                StatusCode::OK
            );
            with_key["invite"] = json!("wrong-key");
            assert_eq!(
                api.write("POST", path, Some(&with_key)).await?.0,
                StatusCode::OK
            );
        }
        let open = TemplateApi::start(None).await?;
        for (path, mut body) in [
            ("/v1/groups", group_body()),
            ("/v1/groups/dev/attach", json!({"channel": "#one"})),
        ] {
            body["invite"] = json!("template-owner-key");
            assert_eq!(
                open.write("POST", path, Some(&body)).await?.0,
                StatusCode::FORBIDDEN
            );
        }
        assert!(open.server.groups().list()?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn groups_http_rejects_invalid_inputs_and_surfaces_storage_failures() -> Result<()> {
        let api = spawn_api(Some("template-owner-key")).await?;
        api.write("POST", "/v1/groups", Some(&group_body())).await?;
        let before = api.server.groups().list()?;
        for (pointer, invalid) in [
            ("/name", json!(" ")),
            ("/name", json!("bad/name")),
            ("/lead/count", json!(2)),
            ("/workers/0/count", json!(0)),
            ("/workers", json!([])),
            ("/lead/model", json!("two words")),
            ("/workers/0/template", json!("../bad")),
        ] {
            let mut input = group_body();
            *input.pointer_mut(pointer).unwrap() = invalid;
            let (status, body) = api.write("POST", "/v1/groups", Some(&input)).await?;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            assert_eq!(body["ok"], false);
            assert_eq!(api.server.groups().list()?, before);
        }
        for (name, channel, expected) in [
            ("dev", "one", StatusCode::BAD_REQUEST),
            ("dev", "#missing", StatusCode::NOT_FOUND),
            ("dev", "#general", StatusCode::NOT_FOUND),
            ("missing", "#one", StatusCode::NOT_FOUND),
        ] {
            let (status, body) = api
                .write(
                    "POST",
                    &format!("/v1/groups/{name}/attach"),
                    Some(&json!({"channel": channel})),
                )
                .await?;
            assert_eq!(status, expected, "{body}");
            assert_eq!(api.server.groups().list()?, before);
        }
        for invalid in [json!({"name": "missing-lead"}), {
            let mut body = group_body();
            body["workers"][0]["count"] = json!(-1);
            body
        }] {
            let response = api
                .client
                .post(format!("{}/v1/groups", api.base))
                .header("X-AirCD-Invite", "template-owner-key")
                .json(&invalid)
                .send()
                .await?;
            assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
            assert_eq!(api.server.groups().list()?, before);
        }
        api.db
            .lock()
            .unwrap()
            .execute("UPDATE groups SET lead_json = 'corrupt'", [])?;
        assert_eq!(
            api.read("/v1/groups").await?.0,
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            api.write(
                "POST",
                "/v1/groups/dev/attach",
                Some(&json!({"channel": "#one"}))
            )
            .await?
            .0,
            StatusCode::INTERNAL_SERVER_ERROR
        );
        Ok(())
    }

    async fn spawn_api(owner_key: Option<&str>) -> Result<TemplateApi> {
        let api = TemplateApi::start(owner_key).await?;
        api.server
            .templates()
            .upsert("example", serde_json::from_value(template_body())?)?;
        let owner = api.server.auth().set_role("human", "leader")?;
        api.server.create_project_api(&owner, "#one", "One", None)?;
        api.server.create_project_api(&owner, "#two", "Two", None)?;
        Ok(api)
    }

    fn provisioning_counts(api: &TemplateApi) -> Result<Vec<i64>> {
        let db = api.db.lock().unwrap();
        [
            "principals",
            "principal_tokens",
            "wake_configs",
            "project_roles",
            "channel_memberships",
            "wake_runs",
        ]
        .iter()
        .map(|table| Ok(db.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?))
        .collect()
    }

    #[tokio::test]
    async fn templates_spawn_http_scopes_pm_permissions_to_every_project() -> Result<()> {
        let api = spawn_api(Some("template-owner-key")).await?;
        let (pm, pm_token) = api.server.auth().register_principal("pm", None, None)?;
        let (outsider, outsider_token) =
            api.server
                .auth()
                .register_principal("outsider", Some("coordinator"), None)?;
        api.server
            .projects()
            .set_role("#one", &pm.id, "coordinator")?;
        api.server
            .projects()
            .set_role("#one", &outsider.id, "builder")?;
        let owner = api.server.auth().authenticate("human-token")?;
        let room = api.server.open_room_api(&owner, "#one", None)?;
        let path = "/v1/templates/example/spawn";
        let request = json!({"nick": "new-worker", "model": "chosen", "channels": ["#one"]});
        let before = provisioning_counts(&api)?;
        for (token, status) in [
            (None, StatusCode::UNAUTHORIZED),
            (Some("invalid-token"), StatusCode::UNAUTHORIZED),
            (Some(api.token.as_str()), StatusCode::FORBIDDEN),
            (Some(outsider_token.as_str()), StatusCode::FORBIDDEN),
        ] {
            let (actual, body) = api
                .request("POST", path, token, None, Some(&request))
                .await?;
            assert_eq!(actual, status, "{body}");
        }
        for channels in [
            json!(["#two"]),
            json!(["#one", "#two"]),
            json!([]),
            json!(["#chat"]),
        ] {
            let mut body = request.clone();
            body["channels"] = channels;
            let (status, body) = api
                .request("POST", path, Some(&pm_token), None, Some(&body))
                .await?;
            assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        }
        assert_eq!(provisioning_counts(&api)?, before);

        let (status, body) = api
            .request(
                "POST",
                path,
                Some(&pm_token),
                None,
                Some(&json!({"nick": "new-worker", "model": "chosen",
                         "channels": [room.channel, "one", "#one"], "role": "admin"})),
            )
            .await?;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["account"]["principal"]["role"], "member");
        assert_eq!(
            api.server
                .projects()
                .role_of("#one", "new-worker")?
                .as_deref(),
            Some("admin")
        );
        assert_eq!(api.server.projects().role_of("#two", "new-worker")?, None);
        assert_eq!(body["account"]["channels"].as_array().unwrap().len(), 3);
        let (_, me) = api
            .request(
                "GET",
                "/v1/me",
                body["account"]["token"].as_str(),
                None,
                None,
            )
            .await?;
        assert_eq!(me["principal"]["id"], "new-worker");
        let memberships: Vec<String> = {
            let db = api.db.lock().unwrap();
            let mut statement = db.prepare(
                "SELECT channel FROM channel_memberships WHERE principal_id = 'new-worker' ORDER BY channel",
            )?;
            let channels = statement
                .query_map([], |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            channels
        };
        assert_eq!(
            memberships,
            vec!["#general".to_string(), "#one".to_string(), room.channel]
        );
        let principal = api.server.auth().resolve_by_name("new-worker")?.unwrap();
        assert!(!Server::is_owner(&principal));
        assert!(!api.server.is_project_manager(&principal, "#two")?);

        // A second project is allowed only once the actor manages it too.
        api.server.projects().set_role("#two", &pm.id, "leader")?;
        let (status, body) = api
            .request(
                "POST",
                path,
                Some(&pm_token),
                None,
                Some(&json!({"nick": "both", "model": "chosen", "channels": ["one", "two"]})),
            )
            .await?;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        for project in ["#one", "#two"] {
            assert_eq!(
                api.server.projects().role_of(project, "both")?.as_deref(),
                Some("member")
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn templates_spawn_http_accepts_owner_keys_and_owner_bearers() -> Result<()> {
        let api = spawn_api(Some("template-owner-key")).await?;
        let path = "/v1/templates/example/spawn";
        let request = json!({"nick": "default-worker", "model": "chosen"});
        let before = provisioning_counts(&api)?;
        for (key, body) in [
            (Some("wrong"), request.clone()),
            (
                Some("wrong"),
                json!({"invite": "template-owner-key", "nick": "worker", "model": "chosen"}),
            ),
            (
                None,
                json!({"invite": "wrong", "nick": "worker", "model": "chosen"}),
            ),
        ] {
            let (status, body) = api
                .request("POST", path, Some("human-token"), key, Some(&body))
                .await?;
            assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        }
        assert_eq!(provisioning_counts(&api)?, before);
        let (status, body) = api.write("POST", path, Some(&request)).await?;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["account"]["channels"], json!(["#general"]));
        assert_eq!(body["account"]["role"], "member");
        let (status, body) = api.request("POST", path, None, None,
            Some(&json!({"invite": "template-owner-key", "nick": "body-key", "model": "chosen", "channels": null}))).await?;
        assert_eq!(status, StatusCode::CREATED, "{body}");

        let open_api = spawn_api(None).await?;
        let (status, _) = open_api.write("POST", path, Some(&request)).await?;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _) = open_api
            .request("POST", path, None, None, Some(&request))
            .await?;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, body) = open_api
            .request(
                "POST",
                path,
                Some("human-token"),
                None,
                Some(&json!({"nick": "owner-bearer", "model": "chosen", "channels": ["lounge"]})),
            )
            .await?;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["account"]["channels"], json!(["#general", "#lounge"]));
        Ok(())
    }

    #[tokio::test]
    async fn templates_spawn_http_substitutes_models_and_preserves_launcher_settings() -> Result<()>
    {
        let api = spawn_api(Some("template-owner-key")).await?;
        for mode in ["fresh", "resume", "pm"] {
            let input = json!({
                "name": "Custom", "command": "aircd-not-installed-{model}",
                "args": ["--model", "{model}", "{prompt}", "{model}/{model}"],
                "cwd": "{repo_path}/{model}",
                "env": {"MODEL": "{model}", "REMOVE": null, "NICK": "{nick}"},
                "prompt": "Use {model}; wake {nick} for {reason} in {repo_path}",
                "session_mode": mode, "session_scope": "channel",
                "session_marker": "session id:",
                "resume_args": ["resume", "{session_id}", "--model={model}", "{prompt}"],
                "models": ["listed-model"],
                "models_command": ["aircd-not-installed-lister", "{model}"]
            });
            let source = api
                .server
                .templates()
                .upsert("custom", serde_json::from_value(input)?)?;
            let nick = format!("worker-{mode}");
            let (status, body) = api.write("POST", "/v1/templates/custom/spawn",
                Some(&json!({"nick": nick, "model": " chosen-v2 ", "channels": ["one"], "role": "builder"}))).await?;
            assert_eq!(status, StatusCode::CREATED, "{body}");
            let account = &body["account"];
            assert_eq!(account["model"], "chosen-v2");
            assert_eq!(account["template"], "custom");
            let wake = api.server.wakes().get(&nick)?.unwrap();
            assert_eq!(serde_json::to_value(&wake)?, account["wake"]);
            assert_eq!(wake.command, "aircd-not-installed-chosen-v2");
            assert_eq!(
                wake.args,
                ["--model", "chosen-v2", "{prompt}", "chosen-v2/chosen-v2"]
            );
            assert_eq!(wake.cwd.as_deref(), Some("{repo_path}/chosen-v2"));
            assert_eq!(wake.env["MODEL"].as_deref(), Some("chosen-v2"));
            assert_eq!(wake.env["NICK"].as_deref(), Some("{nick}"));
            assert_eq!(wake.env["REMOVE"], None);
            assert_eq!(
                wake.prompt.as_deref(),
                Some("Use chosen-v2; wake {nick} for {reason} in {repo_path}")
            );
            assert_eq!(wake.session_mode, mode);
            assert_eq!(wake.session_scope, "channel");
            assert_eq!(wake.session_marker.as_deref(), Some("session id:"));
            assert_eq!(
                wake.resume_args,
                ["resume", "{session_id}", "--model=chosen-v2", "{prompt}"]
            );
            assert!(wake.enabled);
            assert_eq!(wake.cooldown_seconds, 20);
            assert_eq!(wake.timeout_seconds, 1800);
            assert_eq!(wake.max_runs_per_hour, 30);
            let token = account["token"].as_str().unwrap();
            assert!(token.starts_with("airc_"));
            assert_eq!(api.server.auth().authenticate(token)?.id, nick);
            assert_eq!(api.server.templates().get("custom")?.unwrap(), source);
            let stored_token: String = api.db.lock().unwrap().query_row(
                "SELECT token FROM principals WHERE id = ?1",
                [&nick],
                |r| r.get(0),
            )?;
            assert!(stored_token.starts_with("__hashed__:"));
            assert_ne!(stored_token, token);
        }
        assert!(api.server.wakes().runs(None, 10)?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn templates_spawn_http_rejects_invalid_input_without_mutation() -> Result<()> {
        let api = spawn_api(Some("template-owner-key")).await?;
        let path = "/v1/templates/example/spawn";
        let before = provisioning_counts(&api)?;
        for (field, value) in [
            ("nick", json!("")),
            ("nick", json!("#bad")),
            ("nick", json!("bad nick")),
            ("model", json!("")),
            ("model", json!("a b")),
            ("model", json!("{prompt}")),
            ("model", json!("x".repeat(101))),
            ("model", json!("bad\u{0}")),
            ("role", json!("bad role")),
            ("channels", json!([""])),
            ("channels", json!(["#"])),
            ("channels", json!(["bad channel"])),
            ("channels", json!(["#one", "#bad,other"])),
            ("channels", json!(["bad\nchannel"])),
        ] {
            let mut request = json!({"nick": "ghost", "model": "chosen", "channels": ["#one"]});
            request[field] = value;
            let (status, body) = api.write("POST", path, Some(&request)).await?;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{field}: {body}");
            assert_eq!(provisioning_counts(&api)?, before);
        }
        for body in [
            json!({"model": "chosen"}),
            json!({"nick": "ghost"}),
            json!({"nick": "ghost", "model": null}),
            json!({"nick": "ghost", "model": "chosen", "channels": "#one"}),
            json!({"nick": "ghost", "model": "chosen", "role": 3}),
        ] {
            let response = api
                .client
                .post(format!("{}{path}", api.base))
                .header("X-AirCD-Invite", "template-owner-key")
                .json(&body)
                .send()
                .await?;
            assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        }
        let response = api
            .client
            .post(format!("{}{path}", api.base))
            .header("X-AirCD-Invite", "template-owner-key")
            .header("Content-Type", "application/json")
            .body("{")
            .send()
            .await?;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(provisioning_counts(&api)?, before);
        let (status, _) = api
            .write(
                "POST",
                "/v1/templates/missing/spawn",
                Some(&json!({"nick": "ghost", "model": "chosen"})),
            )
            .await?;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(provisioning_counts(&api)?, before);
        Ok(())
    }

    #[tokio::test]
    async fn templates_spawn_http_keeps_duplicate_accounts_and_tokens_unchanged() -> Result<()> {
        let api = spawn_api(Some("template-owner-key")).await?;
        let path = "/v1/templates/example/spawn";
        let body = json!({"nick": "Same-Worker", "model": "chosen", "channels": ["#one"]});
        let (first, second) = tokio::join!(
            api.write("POST", path, Some(&body)),
            api.write("POST", path, Some(&body)),
        );
        let mut results = [first?, second?];
        results.sort_by_key(|(status, _)| status.as_u16());
        assert_eq!(results[0].0, StatusCode::CREATED);
        assert_eq!(results[1].0, StatusCode::CONFLICT);
        let token = results[0].1["account"]["token"].as_str().unwrap();
        let before = provisioning_counts(&api)?;
        let (status, _) = api
            .write(
                "POST",
                path,
                Some(&json!({"nick": "same-worker", "model": "different", "channels": ["#two"]})),
            )
            .await?;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(provisioning_counts(&api)?, before);
        assert_eq!(api.server.auth().authenticate(token)?.id, "same-worker");
        assert_eq!(api.server.projects().role_of("#two", "same-worker")?, None);
        assert_eq!(
            api.server.wakes().get("same-worker")?.unwrap().command,
            "aircd-uninstalled-example-client"
        );
        Ok(())
    }

    #[tokio::test]
    async fn templates_spawn_http_rolls_back_every_provisioning_write_on_failure() -> Result<()> {
        let api = spawn_api(Some("template-owner-key")).await?;
        let path = "/v1/templates/example/spawn";
        let before = provisioning_counts(&api)?;
        let request = json!({"nick": "rollback-worker", "model": "chosen", "channels": ["#one"]});
        for table in [
            "principal_tokens",
            "wake_configs",
            "project_roles",
            "channel_memberships",
        ] {
            api.db.lock().unwrap().execute_batch(&format!(
                "CREATE TRIGGER fail_provision BEFORE INSERT ON {table}
                 BEGIN SELECT RAISE(ABORT, 'injected provisioning failure'); END;"
            ))?;
            let (status, body) = api.write("POST", path, Some(&request)).await?;
            assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{table}: {body}");
            api.db
                .lock()
                .unwrap()
                .execute_batch("DROP TRIGGER fail_provision")?;
            assert_eq!(provisioning_counts(&api)?, before, "{table}");
            assert!(api
                .server
                .auth()
                .resolve_by_name("rollback-worker")?
                .is_none());
        }
        api.db.lock().unwrap().execute(
            "UPDATE client_templates SET command = '' WHERE id = 'example'",
            [],
        )?;
        let (status, _) = api.write("POST", path, Some(&request)).await?;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(provisioning_counts(&api)?, before);
        api.db.lock().unwrap().execute(
            "UPDATE client_templates SET command = 'example-client', args_json = 'corrupt' WHERE id = 'example'", [])?;
        let (status, _) = api.write("POST", path, Some(&request)).await?;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(provisioning_counts(&api)?, before);
        Ok(())
    }

    #[tokio::test]
    async fn templates_spawn_http_provisions_from_all_builtin_fixtures() -> Result<()> {
        let api = TemplateApi::start(Some("template-owner-key")).await?;
        for (id, fixture) in [
            ("claude", include_str!("../docs/templates/claude.json")),
            ("codex", include_str!("../docs/templates/codex.json")),
            ("agy", include_str!("../docs/templates/agy.json")),
            ("lmstudio", include_str!("../docs/templates/lmstudio.json")),
            ("bionic", include_str!("../docs/templates/bionic.json")),
        ] {
            let input: Value = serde_json::from_str(fixture)?;
            let (status, body) = api
                .write("POST", &format!("/v1/templates/{id}"), Some(&input))
                .await?;
            assert_eq!(status, StatusCode::OK, "{body}");
            let (status, body) = api
                .write(
                    "POST",
                    &format!("/v1/templates/{id}/spawn"),
                    Some(&json!({"nick": format!("fixture-{id}"), "model": "user-chosen-model"})),
                )
                .await?;
            assert_eq!(status, StatusCode::CREATED, "{body}");
            assert_eq!(body["account"]["wake"]["command"], input["command"]);
            assert_eq!(body["account"]["wake"]["session_mode"], "fresh");
            assert!(body["account"]["wake"]["args"]
                .as_array()
                .unwrap()
                .contains(&json!("user-chosen-model")));
            assert!(body["account"]["wake"]["args"]
                .as_array()
                .unwrap()
                .contains(&json!("{prompt}")));
            if id == "claude" {
                assert_eq!(body["account"]["wake"]["env"], json!({"CLAUDECODE": null}));
            }
        }
        assert!(api.server.wakes().runs(None, 10)?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn templates_http_authenticates_reads_and_requires_the_owner_key_for_writes() -> Result<()>
    {
        let api = TemplateApi::start(Some("template-owner-key")).await?;
        let path = "/v1/templates/example";
        api.write("POST", path, Some(&template_body())).await?;
        let (_, owner_token) =
            api.server
                .auth()
                .register_principal("owner", Some("leader"), None)?;

        for read_path in ["/v1/templates", path] {
            for token in [None, Some("invalid-token")] {
                let (status, body) = api.request("GET", read_path, token, None, None).await?;
                assert_eq!(status, StatusCode::UNAUTHORIZED, "{read_path}: {body}");
                assert_eq!(body["ok"], false);
            }
            // Owner credentials do not replace an account token on read endpoints.
            let (status, _) = api
                .request("GET", read_path, None, Some("template-owner-key"), None)
                .await?;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
            assert_eq!(api.read(read_path).await?.0, StatusCode::OK);
        }

        for method in ["POST", "DELETE"] {
            let body = (method == "POST").then(template_body);
            for token in [None, Some(api.token.as_str()), Some(owner_token.as_str())] {
                for key in [None, Some("wrong-key")] {
                    let (status, response) =
                        api.request(method, path, token, key, body.as_ref()).await?;
                    assert_eq!(status, StatusCode::FORBIDDEN, "{method}: {response}");
                    assert_eq!(response["ok"], false);
                }
            }
            assert!(api.server.templates().get("example")?.is_some());
        }

        // POST accepts the body key, and the header takes precedence over it.
        let mut body = template_body();
        body["invite"] = json!("template-owner-key");
        assert_eq!(
            api.request("POST", path, None, None, Some(&body)).await?.0,
            StatusCode::OK
        );
        assert_eq!(
            api.request("POST", path, None, Some("wrong-key"), Some(&body))
                .await?
                .0,
            StatusCode::FORBIDDEN
        );
        body["invite"] = json!("wrong-key");
        assert_eq!(
            api.write("POST", path, Some(&body)).await?.0,
            StatusCode::OK
        );
        assert_eq!(api.write("DELETE", path, None).await?.0, StatusCode::OK);
        Ok(())
    }

    #[tokio::test]
    async fn templates_http_refuses_writes_without_a_configured_owner_key() -> Result<()> {
        let api = TemplateApi::start(None).await?;
        for method in ["POST", "DELETE"] {
            let mut body = template_body();
            body["invite"] = json!("template-owner-key");
            let (status, response) = api
                .write(method, "/v1/templates/example", Some(&body))
                .await?;
            assert_eq!(status, StatusCode::FORBIDDEN, "{response}");
        }
        assert_eq!(api.read("/v1/templates").await?.1["templates"], json!([]));
        Ok(())
    }

    #[tokio::test]
    async fn templates_http_crud_preserves_settings_and_replaces_mutable_fields() -> Result<()> {
        let api = TemplateApi::start(Some("template-owner-key")).await?;
        assert_eq!(
            api.read("/v1/templates").await?.1,
            json!({"ok": true, "templates": []})
        );
        let path = "/v1/templates/Example_1";
        let input = json!({
            "name": " Example Client — 日本語 ",
            "command": " aircd-uninstalled-example-client ",
            "args": ["--model={model}", "{prompt}", "", "O'Brien says \"hi\"\n$literal `text`"],
            "cwd": "{repo_path}",
            "env": {"KEEP": "{model}", "REMOVE": null},
            "prompt": "Keep {model} and {unknown} literal\n",
            "session_mode": "resume",
            "resume_args": ["--resume", "{session_id}"],
            "session_marker": "conversation_id:",
            "session_scope": "channel"
        });
        let (status, body) = api.write("POST", path, Some(&input)).await?;
        assert_eq!(status, StatusCode::OK, "{body}");
        let saved = &body["template"];
        assert_eq!(saved["id"], "Example_1");
        assert_eq!(saved["name"], "Example Client — 日本語");
        assert_eq!(saved["command"], "aircd-uninstalled-example-client");
        for field in [
            "args",
            "cwd",
            "env",
            "prompt",
            "session_mode",
            "resume_args",
            "session_marker",
            "session_scope",
        ] {
            assert_eq!(saved[field], input[field], "{field}");
        }
        assert_eq!(saved["installed"], false);
        assert!(saved["created_at"].is_i64());
        assert!(saved["updated_at"].is_i64());
        assert!(saved.get("invite").is_none());
        assert_eq!(api.read(path).await?.1["template"], *saved);
        assert_eq!(
            api.read("/v1/templates").await?.1["templates"],
            json!([saved])
        );

        let replacement = json!({"name": "Replacement", "command": "other-uninstalled-client"});
        let (status, updated) = api.write("POST", path, Some(&replacement)).await?;
        assert_eq!(status, StatusCode::OK);
        let updated = &updated["template"];
        assert_eq!(updated["name"], replacement["name"]);
        assert_eq!(updated["command"], replacement["command"]);
        assert_eq!(updated["created_at"], saved["created_at"]);
        assert!(updated["updated_at"].as_i64() >= saved["updated_at"].as_i64());
        for field in ["args", "resume_args"] {
            assert_eq!(updated[field], json!([]));
        }
        assert_eq!(updated["env"], json!({}));
        for field in ["cwd", "prompt", "session_marker"] {
            assert!(updated[field].is_null());
        }
        assert_eq!(updated["session_mode"], "fresh");
        assert_eq!(updated["session_scope"], "account");
        assert_eq!(
            api.server.templates().get("Example_1")?.unwrap().name,
            "Replacement"
        );

        api.write("POST", "/v1/templates/z-client", Some(&template_body()))
            .await?;
        api.write("POST", "/v1/templates/a-client", Some(&template_body()))
            .await?;
        let listed = api.read("/v1/templates").await?.1;
        let ids: Vec<_> = listed["templates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["Example_1", "a-client", "z-client"]);
        assert_eq!(
            api.read("/v1/templates/example_1").await?.0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            api.write("DELETE", path, None).await?,
            (StatusCode::OK, json!({"ok": true, "deleted": true}))
        );
        assert_eq!(api.read(path).await?.0, StatusCode::NOT_FOUND);
        assert_eq!(
            api.write("DELETE", path, None).await?.0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(api.server.templates().list()?.len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn templates_http_rejects_invalid_input_without_changing_existing_templates() -> Result<()>
    {
        let api = TemplateApi::start(Some("template-owner-key")).await?;
        let path = "/v1/templates/example";
        let saved = api.write("POST", path, Some(&template_body())).await?.1;
        for input in [
            json!({}),
            json!({"name": "Example"}),
            json!({"command": "example"}),
            json!({"name": " ", "command": "example"}),
            json!({"name": "Example", "command": " "}),
            json!({"name": "Bad\u{0}name", "command": "example"}),
            json!({"name": "Example", "command": "bad\u{0}command"}),
            json!({"name": "Example", "command": "example", "session_mode": "invalid"}),
            json!({"name": "Example", "command": "example", "session_scope": "invalid"}),
        ] {
            let (status, body) = api.write("POST", path, Some(&input)).await?;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{input}: {body}");
            assert_eq!(body["ok"], false);
            assert_eq!(api.read(path).await?.1, saved);
        }
        for bad_id in ["bad.id", "bad%20id", "%E6%97%A5%E6%9C%AC", "%20"] {
            let path = format!("/v1/templates/{bad_id}");
            assert_eq!(
                api.write("POST", &path, Some(&template_body())).await?.0,
                StatusCode::BAD_REQUEST
            );
            assert_eq!(api.read(&path).await?.0, StatusCode::BAD_REQUEST);
            assert_eq!(
                api.write("DELETE", &path, None).await?.0,
                StatusCode::BAD_REQUEST
            );
        }

        // Invalid JSON types fail extraction rather than silently dropping settings.
        for (field, value) in [
            ("args", json!("not-an-array")),
            ("args", json!([1])),
            ("env", json!({"KEY": 123})),
            ("command", Value::Null),
            ("session_mode", json!(true)),
        ] {
            let mut input = template_body();
            input[field] = value;
            let response = api
                .client
                .post(format!("{}{path}", api.base))
                .header("X-AirCD-Invite", "template-owner-key")
                .json(&input)
                .send()
                .await?;
            assert_eq!(
                response.status(),
                StatusCode::UNPROCESSABLE_ENTITY,
                "{input}"
            );
        }
        let malformed = api
            .client
            .post(format!("{}{path}", api.base))
            .header("X-AirCD-Invite", "template-owner-key")
            .header(header::CONTENT_TYPE, "application/json")
            .body("{")
            .send()
            .await?;
        assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);
        assert_eq!(api.read(path).await?.1, saved);
        assert_eq!(api.server.templates().list()?.len(), 1);
        Ok(())
    }

    struct TemplateProbeDir(std::path::PathBuf);

    impl TemplateProbeDir {
        fn new() -> Result<Self> {
            let path = std::env::temp_dir().join(format!(
                "aircd-template-probe-{} with spaces",
                uuid::Uuid::new_v4()
            ));
            std::fs::create_dir(&path)?;
            Ok(Self(path))
        }
    }

    impl Drop for TemplateProbeDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn templates_http_detects_paths_and_path_commands_at_request_time_without_launching(
    ) -> Result<()> {
        let api = TemplateApi::start(Some("template-owner-key")).await?;
        let dir = TemplateProbeDir::new()?;
        let executable = dir.0.join(if cfg!(windows) {
            "client.cmd"
        } else {
            "client"
        });
        let marker = dir.0.join("must-not-be-created");
        let script = if cfg!(windows) {
            format!("@echo executed>\"{}\"\r\n", marker.display())
        } else {
            format!("#!/bin/sh\nprintf executed > '{}'\n", marker.display())
        };
        std::fs::write(&executable, &script)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))?;
        }
        let path = "/v1/templates/probe";
        let (status, created) = api
            .write(
                "POST",
                path,
                Some(&json!({
                    "name": "Probe", "command": executable.to_str().unwrap()
                })),
            )
            .await?;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(created["template"]["installed"], true);
        assert_eq!(api.read(path).await?.1["template"]["installed"], true);
        assert_eq!(
            api.read("/v1/templates").await?.1["templates"][0]["installed"],
            true
        );
        std::fs::remove_file(&executable)?;
        assert_eq!(api.read(path).await?.1["template"]["installed"], false);
        assert_eq!(
            api.read("/v1/templates").await?.1["templates"][0]["installed"],
            false
        );
        std::fs::write(&executable, &script)?;
        assert_eq!(api.read(path).await?.1["template"]["installed"], true);
        assert!(
            !marker.exists(),
            "availability checks must never execute the command"
        );

        // A directory or missing file is not a client executable.
        for command in [dir.0.clone(), dir.0.join("missing-client")] {
            let (_, response) = api
                .write(
                    "POST",
                    path,
                    Some(&json!({
                        "name": "Probe", "command": command.to_str().unwrap()
                    })),
                )
                .await?;
            assert_eq!(response["template"]["installed"], false);
        }
        // Known host commands cover PATH lookup (including .exe inference on Windows).
        let command = if cfg!(windows) { "cmd" } else { "sh" };
        let (_, response) = api
            .write(
                "POST",
                path,
                Some(&json!({"name": "Host shell", "command": command})),
            )
            .await?;
        assert_eq!(
            response["template"]["installed"], true,
            "{command} must be on the test host PATH"
        );
        assert_eq!(api.read(path).await?.1["template"]["installed"], true);
        assert!(!marker.exists());
        Ok(())
    }

    #[tokio::test]
    async fn templates_http_reports_storage_failures_as_server_errors() -> Result<()> {
        let api = TemplateApi::start(Some("template-owner-key")).await?;
        let path = "/v1/templates/example";
        api.write("POST", path, Some(&template_body())).await?;
        api.db
            .lock()
            .unwrap()
            .execute("UPDATE client_templates SET args_json = 'corrupt'", [])?;
        for read_path in ["/v1/templates", path] {
            let (status, body) = api.read(read_path).await?;
            assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
            assert_eq!(body["ok"], false);
        }
        api.db
            .lock()
            .unwrap()
            .execute_batch("DROP TABLE client_templates")?;
        for (method, read_path, body) in [
            ("GET", "/v1/templates", None),
            ("GET", path, None),
            ("POST", path, Some(template_body())),
            ("DELETE", path, None),
        ] {
            let (status, body) = api
                .request(
                    method,
                    read_path,
                    Some(&api.token),
                    Some("template-owner-key"),
                    body.as_ref(),
                )
                .await?;
            assert_eq!(
                status,
                StatusCode::INTERNAL_SERVER_ERROR,
                "{method} {read_path}: {body}"
            );
            assert_eq!(body["ok"], false);
        }
        Ok(())
    }

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
            http_tls_port: None,
            tls_port: None,
            tls_client_auth: "none".to_string(),
            tls_only: false,
            detected_hosts: vec!["127.0.0.1".to_string()],
            register_key: None,
            rate_limit: RateLimitConfig::default(),
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
    async fn task_reviewers_default_to_parent_lead_or_pm_and_validate_input() -> Result<()> {
        let (api, tokens) = task_workflow_api().await?;
        let post = |body: Value| {
            call(
                &api.client,
                "POST",
                format!("{}/v1/tasks", api.base),
                &tokens["pm"],
                body,
            )
        };
        let (status, body) =
            post(json!({"channel": "#one", "title": "Parent", "assignee": "lead"})).await?;
        assert_eq!(status, 201, "{body}");
        assert_eq!(body["task"]["reviewer"], "pm");
        assert!(body["task"]["parent_task_id"].is_null());
        let parent = body["task"]["id"].as_str().unwrap();
        let child_input = json!({"channel": "#one", "title": "Child", "assignee": "worker", "parent_task_id": parent});
        let (status, body) = call(
            &api.client,
            "POST",
            format!("{}/v1/tasks", api.base),
            &tokens["lead"],
            child_input.clone(),
        )
        .await?;
        assert_eq!(status, 201, "{body}");
        assert_eq!(body["task"]["reviewer"], "lead");
        assert_eq!(body["task"]["parent_task_id"], parent);
        let child = body["task"]["id"].as_str().unwrap();

        // The lead can reassign its child, without acquiring project-wide authority.
        let (status, _) = call(
            &api.client,
            "POST",
            format!("{}/v1/tasks/{child}/assign", api.base),
            &tokens["lead"],
            json!({"assignee": "peer"}),
        )
        .await?;
        assert_eq!(status, 200);
        let (status, _) = call(
            &api.client,
            "POST",
            format!("{}/v1/tasks/{parent}/assign", api.base),
            &tokens["worker"],
            json!({"assignee": "peer"}),
        )
        .await?;
        assert_eq!(status, 400);
        let (status, _) = call(
            &api.client,
            "POST",
            format!("{}/v1/tasks", api.base),
            &tokens["lead"],
            json!({"channel": "#one", "title": "Unrelated", "assignee": "worker"}),
        )
        .await?;
        assert_eq!(status, 400);

        let mut explicit = child_input.clone();
        explicit["reviewer"] = json!(" @Peer ");
        let (status, body) = post(explicit).await?;
        assert_eq!(status, 201, "{body}");
        assert_eq!(body["task"]["reviewer"], "peer");

        let (_, body) = post(json!({"channel": "#one", "title": "Unassigned parent"})).await?;
        let unassigned = body["task"]["id"].as_str().unwrap();
        let (status, body) =
            post(json!({"channel": "#one", "title": "Fallback", "parent_task_id": unassigned}))
                .await?;
        assert_eq!(status, 201, "{body}");
        assert_eq!(body["task"]["reviewer"], "pm");
        let owner = api.server.auth().resolve_by_name("human")?.unwrap();
        let (status, body) = call(
            &api.client,
            "POST",
            format!("{}/v1/tasks", api.base),
            "human-token",
            json!({"channel": "#two", "title": "No PM"}),
        )
        .await?;
        assert_eq!(status, 201, "{body}");
        assert!(body["task"]["reviewer"].is_null());
        let other_project = body["task"]["id"].as_str().unwrap();

        let before = api.server.tasks().list("#one")?.len();
        for (field, value) in [
            ("parent_task_id", json!("missing")),
            ("parent_task_id", json!(other_project)),
            ("reviewer", json!("missing")),
            ("reviewer", json!("reader")), // existing account, outside the project
        ] {
            let mut invalid = child_input.clone();
            invalid[field] = value;
            let (status, body) = post(invalid).await?;
            assert_eq!(status, 400, "{body}");
            assert_eq!(api.server.tasks().list("#one")?.len(), before);
        }
        let (status, _) = call(&api.client, "POST", format!("{}/v1/tasks", api.base), &tokens["worker"], json!({"channel": "#one", "title": "Self-selected review", "assignee": "worker", "reviewer": "peer"})).await?;
        assert_eq!(status, 400);
        api.server
            .set_task_status_api(&owner, parent, "cancelled", None, None, None, None)?;
        let (status, _) = post(child_input).await?;
        assert_eq!(status, 400);
        Ok(())
    }

    async fn task_workflow_api(
    ) -> Result<(TemplateApi, std::collections::HashMap<&'static str, String>)> {
        let api = spawn_api(Some("template-owner-key")).await?;
        let owner = api.server.auth().resolve_by_name("human")?.unwrap();
        let mut tokens = std::collections::HashMap::new();
        for nick in ["pm", "lead", "worker", "peer"] {
            let (_, token) = api
                .server
                .auth()
                .register_principal(nick, Some("builder"), None)?;
            api.server
                .set_project_member_api(&owner, "#one", nick, Some("builder"))?;
            tokens.insert(nick, token);
        }
        api.server.set_project_pm_api(&owner, "#one", "pm")?;
        Ok((api, tokens))
    }

    #[tokio::test]
    async fn task_review_requires_evidence_and_routes_to_the_named_lead() -> Result<()> {
        let (api, tokens) = task_workflow_api().await?;
        let (_, body) = call(
            &api.client,
            "POST",
            format!("{}/v1/tasks", api.base),
            &tokens["pm"],
            json!({"channel": "#one", "title": "Parent", "assignee": "lead"}),
        )
        .await?;
        let parent = body["task"]["id"].as_str().unwrap();
        let (_, body) = call(&api.client, "POST", format!("{}/v1/tasks", api.base), &tokens["lead"], json!({"channel": "#one", "title": "Child", "assignee": "worker", "parent_task_id": parent})).await?;
        let id = body["task"]["id"].as_str().unwrap();
        let url = format!("{}/v1/tasks/{id}/status", api.base);
        let (status, _) = call(
            &api.client,
            "POST",
            url.clone(),
            &tokens["worker"],
            json!({"status": "review", "note": "cannot skip testing"}),
        )
        .await?;
        assert_eq!(status, 400);
        let (status, _) = call(
            &api.client,
            "POST",
            url.clone(),
            &tokens["worker"],
            json!({"status": "testing"}),
        )
        .await?;
        assert_eq!(status, 200);
        let before = serde_json::to_value(api.server.get_task_api(id)?)?;
        let event_count = api.server.tasks().events(id)?.len();
        for evidence in [
            json!({}),
            json!({"note": null}),
            json!({"note": ""}),
            json!({"note": " \t\n", "report_body": " \r\n", "report_id": " "}),
        ] {
            let mut request = evidence;
            request["status"] = json!("review");
            request["branch"] = json!("must-not-stick");
            let (status, body) =
                call(&api.client, "POST", url.clone(), &tokens["worker"], request).await?;
            assert_eq!(status, 400, "{body}");
            assert!(body["error"].as_str().unwrap().contains("test report"));
            assert_eq!(serde_json::to_value(api.server.get_task_api(id)?)?, before);
            assert_eq!(api.server.tasks().events(id)?.len(), event_count);
        }
        let report = "cargo test --locked: 8 passed\nresult: ok";
        let (status, body) = call(
            &api.client,
            "POST",
            url.clone(),
            &tokens["worker"],
            json!({"status": "review", "report_body": report, "branch": "feat/child"}),
        )
        .await?;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["task"]["branch"], "feat/child");
        assert_eq!(
            api.server
                .tasks()
                .events(id)?
                .last()
                .unwrap()
                .note
                .as_deref(),
            Some(report)
        );
        for nick in ["lead", "pm"] {
            let (_, body) = call(
                &api.client,
                "GET",
                format!("{}/v1/inbox", api.base),
                &tokens[nick],
                Value::Null,
            )
            .await?;
            assert_eq!(body["inbox"]["review_count"], 1, "{nick}: {body}");
            assert_eq!(body["inbox"]["projects"][0]["to_review"][0]["id"], id);
        }
        let (_, body) = call(
            &api.client,
            "GET",
            format!("{}/v1/inbox", api.base),
            &tokens["peer"],
            Value::Null,
        )
        .await?;
        assert_eq!(body["inbox"]["review_count"], 0);
        let messages = api.server.history().tail("#one", 0, 20)?;
        let notice = messages
            .iter()
            .find(|message| message.body.contains(id) && message.body.contains("testing -> review"))
            .unwrap();
        assert!(notice.body.contains("@lead"));
        assert!(
            !notice.body.contains("@pm"),
            "PM oversight stays in the inbox without a subtask wake"
        );
        for nick in ["worker", "peer"] {
            let (status, _) = call(
                &api.client,
                "POST",
                url.clone(),
                &tokens[nick],
                json!({"status": "merged"}),
            )
            .await?;
            assert_eq!(status, 400);
        }
        let (status, _) = call(
            &api.client,
            "POST",
            url.clone(),
            &tokens["lead"],
            json!({"status": "cancelled"}),
        )
        .await?;
        assert_eq!(status, 400);
        let (status, _) = call(
            &api.client,
            "POST",
            url.clone(),
            &tokens["lead"],
            json!({"status": "changes_requested", "note": "fix the test"}),
        )
        .await?;
        assert_eq!(status, 200);
        let (status, _) = call(
            &api.client,
            "POST",
            url.clone(),
            &tokens["worker"],
            json!({"status": "testing"}),
        )
        .await?;
        assert_eq!(status, 200);
        let (status, _) = call(
            &api.client,
            "POST",
            url.clone(),
            &tokens["worker"],
            json!({"status": "review"}),
        )
        .await?;
        assert_eq!(status, 400); // prior evidence does not authorize another handoff
        let (status, _) = call(
            &api.client,
            "POST",
            url.clone(),
            &tokens["worker"],
            json!({"status": "review", "note": "fixed; 9 tests pass"}),
        )
        .await?;
        assert_eq!(status, 200);
        let (status, _) = call(
            &api.client,
            "POST",
            url,
            &tokens["lead"],
            json!({"status": "merged"}),
        )
        .await?;
        assert_eq!(status, 200);
        Ok(())
    }

    #[tokio::test]
    async fn named_task_reviewers_cannot_approve_their_own_work_or_review_after_leaving(
    ) -> Result<()> {
        let (api, tokens) = task_workflow_api().await?;
        for reviewer in ["worker", "peer"] {
            let (status, body) = call(
                &api.client, "POST", format!("{}/v1/tasks", api.base), &tokens["pm"],
                json!({"channel": "#one", "title": "Review permissions", "assignee": "worker", "reviewer": reviewer}),
            ).await?;
            assert_eq!(status, 201, "{body}");
            let id = body["task"]["id"].as_str().unwrap();
            let url = format!("{}/v1/tasks/{id}/status", api.base);
            for request in [
                json!({"status": "testing"}),
                json!({"status": "review", "note": "cargo test: passed"}),
            ] {
                let (status, body) =
                    call(&api.client, "POST", url.clone(), &tokens["worker"], request).await?;
                assert_eq!(status, 200, "{body}");
            }
            if reviewer == "peer" {
                api.db.lock().unwrap().execute(
                    "DELETE FROM project_roles WHERE channel = '#one' AND principal_id = 'peer'",
                    [],
                )?;
            }
            let (_, body) = call(
                &api.client,
                "GET",
                format!("{}/v1/inbox", api.base),
                &tokens[reviewer],
                Value::Null,
            )
            .await?;
            assert_eq!(body["inbox"]["review_count"], 0);
            let (status, _) = call(
                &api.client,
                "POST",
                url.clone(),
                &tokens[reviewer],
                json!({"status": "merged"}),
            )
            .await?;
            assert_eq!(status, 400);
            let (status, _) = call(
                &api.client,
                "POST",
                url,
                &tokens["pm"],
                json!({"status": "merged"}),
            )
            .await?;
            assert_eq!(status, 200);
        }
        Ok(())
    }

    #[tokio::test]
    async fn task_review_accepts_only_the_assignees_task_linked_report_body() -> Result<()> {
        let (api, tokens) = task_workflow_api().await?;
        let (_, body) = call(
            &api.client,
            "POST",
            format!("{}/v1/tasks", api.base),
            &tokens["pm"],
            json!({"channel": "#one", "title": "Test reports", "assignee": "worker"}),
        )
        .await?;
        let id = body["task"]["id"].as_str().unwrap();
        let url = format!("{}/v1/tasks/{id}/status", api.base);
        let (status, _) = call(
            &api.client,
            "POST",
            url.clone(),
            &tokens["worker"],
            json!({"status": "testing"}),
        )
        .await?;
        assert_eq!(status, 200);
        let before = serde_json::to_value(api.server.get_task_api(id)?)?;
        let event_count = api.server.tasks().events(id)?.len();
        let (status, _) = call(
            &api.client,
            "POST",
            url.clone(),
            &tokens["worker"],
            json!({"status": "review", "report_id": "missing"}),
        )
        .await?;
        assert_eq!(status, 400);
        for (author, channel, task_id, body) in [
            ("worker", "#one", Some(id), " \n"),
            ("peer", "#one", Some(id), "8 passed"),
            ("worker", "#one", None, "8 passed"),
            ("worker", "#one", Some("another-task"), "8 passed"),
            ("worker", "#two", Some(id), "8 passed"),
        ] {
            let (_, report) = call(&api.client, "POST", format!("{}/v1/reports", api.base), &tokens[author], json!({"channel": channel, "title": "Tests", "status": "ok", "body": body, "task_id": task_id})).await?;
            let report_id = report["report"]["id"].as_str().unwrap();
            let (status, body) = call(
                &api.client,
                "POST",
                url.clone(),
                &tokens["worker"],
                json!({"status": "review", "report_id": report_id, "branch": "must-not-stick"}),
            )
            .await?;
            assert_eq!(status, 400, "{body}");
            assert_eq!(serde_json::to_value(api.server.get_task_api(id)?)?, before);
            assert_eq!(api.server.tasks().events(id)?.len(), event_count);
        }
        let (_, report) = call(&api.client, "POST", format!("{}/v1/reports", api.base), &tokens["worker"], json!({"channel": "#one", "title": "Tests", "status": "ok", "body": "cargo test: 8 passed\nno failures", "task_id": id})).await?;
        let report_id = report["report"]["id"].as_str().unwrap();
        let (status, body) = call(
            &api.client,
            "POST",
            url,
            &tokens["worker"],
            json!({"status": "review", "report_id": report_id}),
        )
        .await?;
        assert_eq!(status, 200, "{body}");
        let events = api.server.tasks().events(id)?;
        let note = events.last().unwrap().note.as_deref().unwrap();
        assert!(note.contains(report_id));
        assert!(note.contains("cargo test: 8 passed\nno failures"));
        Ok(())
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
        let (status, _) = call(&client, "POST", format!("{base}/v1/gits"), owner, json!({
            "name": "quest-player", "channel": "#quest", "local_path": "/tmp/quest-player",
        })).await?;
        assert_eq!(status, 201);

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
        // Work is tested before it is handed in, and handing it in needs a test report.
        let (status, _) = call(&client, "POST", status_url.clone(), worker,
            json!({"status": "review", "note": "skipping tests"})).await?;
        assert_eq!(status, 400);
        let (status, _) = call(&client, "POST", status_url.clone(), worker,
            json!({"status": "testing"})).await?;
        assert_eq!(status, 200);
        let (status, body) = call(&client, "POST", status_url.clone(), worker,
            json!({"status": "review", "branch": "fix/seek-bar"})).await?;
        assert_eq!(status, 400);
        assert!(body["error"].as_str().unwrap().contains("test report"));
        let (status, body) = call(&client, "POST", status_url.clone(), worker, json!({
            "status": "review", "branch": "fix/seek-bar", "note": "cargo test: 12 passed",
        })).await?;
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
            json!({"status": "testing"})).await?;
        assert_eq!(status, 200);
        let (status, _) = call(&client, "POST", status_url.clone(), worker,
            json!({"status": "review", "note": "added a test; 13 passed"})).await?;
        assert_eq!(status, 200);
        let (status, body) = call(&client, "POST", status_url.clone(), coord,
            json!({"status": "merged"})).await?;
        assert_eq!(status, 200);
        assert_eq!(body["task"]["status"], "merged");

        let (_, body) = call(&client, "GET", format!("{base}/v1/inbox"), worker, Value::Null).await?;
        assert_eq!(body["inbox"]["task_count"], 0);
        let (_, body) = call(&client, "GET", format!("{base}/v1/tasks/{id}"), worker, Value::Null).await?;
        assert_eq!(body["events"].as_array().unwrap().len(), 7);

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
        assert_eq!(body["wake"]["max_runs_per_hour"], 30);

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
    async fn tasks_belong_to_a_sprint_room_and_its_history_records_them() -> Result<()> {
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
        for (nick, role) in [("owner", "leader"), ("worker", "member")] {
            let res: Value = client
                .post(format!("{base}/v1/register"))
                .json(&json!({"nick": nick, "role": role}))
                .send()
                .await?
                .json()
                .await?;
            tokens.insert(nick, res["token"].as_str().unwrap().to_string());
        }
        let (owner, worker) = (tokens["owner"].as_str(), tokens["worker"].as_str());
        call(&client, "POST", format!("{base}/v1/projects"), owner,
            json!({"channel": "#quest"})).await?;
        call(&client, "POST", format!("{base}/v1/projects/members"), owner,
            json!({"channel": "#quest", "nick": "worker"})).await?;
        let tasks_url = format!("{base}/v1/tasks");
        let rooms_url = format!("{base}/v1/rooms");

        // Before any sprint is opened a task has no sprint.
        let (_, body) = call(&client, "POST", tasks_url.clone(), owner,
            json!({"channel": "#quest", "title": "Before sprints"})).await?;
        assert_eq!(body["task"]["sprint"], Value::Null);

        // A plain member cannot open a sprint; the owner can.
        let (status, _) = call(&client, "POST", rooms_url.clone(), worker,
            json!({"project": "#quest"})).await?;
        assert_eq!(status, 400);
        let (status, body) = call(&client, "POST", rooms_url.clone(), owner,
            json!({"project": "#quest", "title": "Local playback"})).await?;
        assert_eq!(status, 201);
        assert_eq!(body["room"]["channel"], "#quest-s1");
        assert_eq!(body["room"]["label"], "s1");
        assert_eq!(body["room"]["title"], "Local playback");

        // New tasks go into the current sprint and stay in their project.
        let (_, body) = call(&client, "POST", tasks_url.clone(), owner,
            json!({"channel": "#quest", "title": "Seek bar", "assignee": "worker"})).await?;
        assert_eq!(body["task"]["channel"], "#quest");
        assert_eq!(body["task"]["sprint"], "#quest-s1");

        call(&client, "POST", rooms_url.clone(), owner, json!({"project": "#quest"})).await?;
        let (_, body) = call(&client, "POST", tasks_url.clone(), owner,
            json!({"channel": "#quest", "title": "Subtitles"})).await?;
        assert_eq!(body["task"]["sprint"], "#quest-s2");
        // Naming a room puts the task in that sprint, even an older one.
        let (_, body) = call(&client, "POST", tasks_url.clone(), owner,
            json!({"channel": "#quest-s1", "title": "Late fix"})).await?;
        assert_eq!(body["task"]["channel"], "#quest");
        assert_eq!(body["task"]["sprint"], "#quest-s1");

        let (_, body) = call(&client, "GET", format!("{rooms_url}?channel=%23quest"), owner,
            Value::Null).await?;
        assert_eq!(body["rooms"].as_array().unwrap().len(), 2);

        // The room's history is the record: it holds the notices of its tasks.
        let history = |room: &str| {
            let client = client.clone();
            let url = format!("{base}/v1/messages?channel={}", room.replace('#', "%23"));
            async move {
                let (_, body) = call(&client, "GET", url, owner, Value::Null).await.unwrap();
                body["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|m| m["body"].as_str().unwrap().to_string())
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        };
        let first = history("#quest-s1").await;
        assert!(first.contains("Seek bar") && first.contains("Late fix"), "{first}");
        assert!(!first.contains("Subtitles"));
        assert!(history("#quest-s2").await.contains("Subtitles"));
        // The assignee, a member of the project, can read the room as well.
        let (status, _) = call(&client, "GET",
            format!("{base}/v1/messages?channel=%23quest-s1"), worker, Value::Null).await?;
        assert_eq!(status, 200);

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
    async fn owner_creates_an_account_from_a_template_and_model() -> Result<()> {
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
        let key = "owner-key-123";

        let res: Value = client
            .post(format!("{base}/v1/register"))
            .json(&json!({"nick": "boss", "role": "leader", "invite": key}))
            .send()
            .await?
            .json()
            .await?;
        let boss = res["token"].as_str().unwrap().to_string();
        let (status, _) = call(&client, "POST", format!("{base}/v1/projects"), &boss,
            json!({"channel": "#quest"})).await?;
        assert_eq!(status, 201);
        let (status, _) = call(&client, "POST", format!("{base}/v1/templates/client-x"), &boss,
            json!({
                "invite": key, "name": "Client X", "command": "aircd-uninstalled-client-x",
                "args": ["--model", "{model}", "{prompt}"],
                "models": ["model-a", "model-b"],
                "models_command": ["aircd-uninstalled-lister", "models"],
            })).await?;
        assert_eq!(status, 200);

        // Listing models and creating accounts need the owner key.
        let (status, _) = call(&client, "POST", format!("{base}/v1/owner/models"), &boss,
            json!({"template": "client-x"})).await?;
        assert_eq!(status, 403);
        let (status, _) = call(&client, "POST", format!("{base}/v1/owner/accounts"), &boss,
            json!({"template": "client-x", "model": "model-b", "channel": "#quest"})).await?;
        assert_eq!(status, 403);

        // The listing command is not installed: the saved list still comes back.
        let (status, body) = call(&client, "POST", format!("{base}/v1/owner/models"), &boss,
            json!({"invite": key, "template": "client-x"})).await?;
        assert_eq!(status, 200);
        assert_eq!(body["models"], json!([
            {"id": "model-a", "label": null}, {"id": "model-b", "label": null},
        ]));
        assert!(body["warning"].as_str().unwrap().contains("aircd-uninstalled-lister"));

        // Nothing is created when the project is wrong.
        let (status, _) = call(&client, "POST", format!("{base}/v1/owner/accounts"), &boss,
            json!({"invite": key, "template": "client-x", "model": "model-a",
                   "nick": "ghost", "channel": "#nope"})).await?;
        assert_eq!(status, 400);
        let (status, _) = call(&client, "POST", format!("{base}/v1/owner/wake"), &boss,
            json!({"invite": key, "action": "show", "nick": "ghost"})).await?;
        assert_eq!(status, 404);

        let (status, body) = call(&client, "POST", format!("{base}/v1/owner/accounts"), &boss,
            json!({"invite": key, "template": "client-x", "model": "model-b",
                   "channel": "quest", "role": "builder"})).await?;
        assert_eq!(status, 201, "{body}");
        let account = &body["account"];
        assert_eq!(account["principal"]["nick"], "model-b");
        assert_eq!(account["channel"], "#quest");
        assert_eq!(account["role"], "builder");
        assert_eq!(account["wake"]["args"], json!(["--model", "model-b", "{prompt}"]));
        assert_eq!(account["wake"]["enabled"], true);
        let token = account["token"].as_str().unwrap().to_string();
        let (status, me) = call(&client, "GET", format!("{base}/v1/me"), &token, Value::Null).await?;
        assert_eq!(status, 200);
        assert_eq!(me["principal"]["nick"], "model-b");

        let (_, members) = call(&client, "GET",
            format!("{base}/v1/projects/members?channel=%23quest"), &boss, Value::Null).await?;
        assert!(members["members"].as_array().unwrap().iter()
            .any(|m| m["principal_id"] == "model-b" && m["role"] == "builder"), "{members}");

        // A second account for the same model needs its own name.
        let (status, body) = call(&client, "POST", format!("{base}/v1/owner/accounts"), &boss,
            json!({"invite": key, "template": "client-x", "model": "model-b",
                   "channel": "#quest"})).await?;
        assert_eq!(status, 400);
        assert!(body["error"].as_str().unwrap().contains("already exists"));
        let (status, body) = call(&client, "POST", format!("{base}/v1/owner/accounts"), &boss,
            json!({"invite": key, "template": "client-x", "model": "model-b",
                   "nick": "model-b-2", "channel": "#quest"})).await?;
        assert_eq!(status, 201, "{body}");
        assert_eq!(body["account"]["role"], "member");
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

    #[tokio::test]
    async fn task_creation_validates_repo_against_project_git_repos() -> Result<()> {
        let (api, tokens) = task_workflow_api().await?;
        let pm = tokens["pm"].as_str();
        let tasks_url = format!("{}/v1/tasks", api.base);
        let gits_url = format!("{}/v1/gits", api.base);

        // No git repos are attached to #one yet -> passing repo fails with HTTP 400.
        let (status, body) = call(
            &api.client,
            "POST",
            tasks_url.clone(),
            pm,
            json!({"channel": "#one", "title": "Needs repo", "repo": "alpha"}),
        )
        .await?;
        assert_eq!(status, 400, "{body}");
        let err = body["error"].as_str().unwrap();
        assert!(err.contains("alpha") && err.contains("#one"), "unexpected error: {err}");

        // Register repo "beta" on #two and "alpha" on #one.
        let (status, _) = call(
            &api.client,
            "POST",
            gits_url.clone(),
            pm,
            json!({"name": "beta", "channel": "#two", "local_path": "/tmp/beta"}),
        )
        .await?;
        assert_eq!(status, 201);

        // Repo registered for #two is rejected when creating a task in #one.
        let (status, body) = call(
            &api.client,
            "POST",
            tasks_url.clone(),
            pm,
            json!({"channel": "#one", "title": "Wrong project repo", "repo": "beta"}),
        )
        .await?;
        assert_eq!(status, 400, "{body}");
        let err = body["error"].as_str().unwrap();
        assert!(err.contains("beta") && err.contains("#two") && err.contains("#one"), "unexpected error: {err}");

        // Register "alpha" on #one.
        let (status, _) = call(
            &api.client,
            "POST",
            gits_url,
            pm,
            json!({"name": "alpha", "channel": "#one", "local_path": "/tmp/alpha"}),
        )
        .await?;
        assert_eq!(status, 201);

        // Unknown repo when #one has "alpha" lists #one's registered repos in HTTP 400.
        let (status, body) = call(
            &api.client,
            "POST",
            tasks_url.clone(),
            pm,
            json!({"channel": "#one", "title": "Missing repo", "repo": "gamma"}),
        )
        .await?;
        assert_eq!(status, 400, "{body}");
        let err = body["error"].as_str().unwrap();
        assert!(err.contains("gamma") && err.contains("alpha"), "unexpected error: {err}");

        // Case-insensitive match succeeds and stores the canonical registered repo name.
        let (status, body) = call(
            &api.client,
            "POST",
            tasks_url.clone(),
            pm,
            json!({"channel": "#one", "title": "Valid repo", "repo": "ALPHA"}),
        )
        .await?;
        assert_eq!(status, 201, "{body}");
        assert_eq!(body["task"]["repo"], "alpha");

        // Sprint rooms resolve to their parent project's registered repos.
        let (status, room_body) = call(
            &api.client,
            "POST",
            format!("{}/v1/rooms", api.base),
            pm,
            json!({"project": "#one", "title": "Sprint 1"}),
        )
        .await?;
        assert_eq!(status, 201, "{room_body}");
        let sprint_channel = room_body["room"]["channel"].as_str().unwrap();
        let (status, body) = call(
            &api.client,
            "POST",
            tasks_url.clone(),
            pm,
            json!({"channel": sprint_channel, "title": "Sprint repo task", "repo": "alpha"}),
        )
        .await?;
        assert_eq!(status, 201, "{body}");
        assert_eq!(body["task"]["channel"], "#one");
        assert_eq!(body["task"]["repo"], "alpha");

        // Omitting or sending blank repo remains allowed.
        let (status, body) = call(
            &api.client,
            "POST",
            tasks_url,
            pm,
            json!({"channel": "#one", "title": "No repo", "repo": "   "}),
        )
        .await?;
        assert_eq!(status, 201, "{body}");
        assert_eq!(body["task"]["repo"], Value::Null);

        Ok(())
    }

    #[test]
    fn runtime_info_lists_only_reachable_hosts() {
        let local = build_runtime_info("127.0.0.1:6667", "127.0.0.1:6680", None);
        assert_eq!(local.detected_hosts, vec!["127.0.0.1".to_string()]);
        let wildcard = build_runtime_info("0.0.0.0:6667", "0.0.0.0:6680", None);
        assert!(wildcard.detected_hosts.contains(&"127.0.0.1".to_string()));
    }

    #[tokio::test]
    async fn http_forwards_security_audits_and_internal_errors_to_syslog() -> Result<()> {
        let syslog_rx = std::net::UdpSocket::bind("127.0.0.1:0")?;
        syslog_rx.set_nonblocking(true)?;
        let syslog_addr = syslog_rx.local_addr()?;

        let forwarder = crate::logging::EventForwarder::new(crate::logging::LogConfig {
            syslog_target: Some(crate::logging::SyslogTarget::Udp(syslog_addr.to_string())),
            syslog_format: crate::logging::SyslogFormat::Rfc5424,
            stderr_errors: false,
            ..crate::logging::LogConfig::default()
        })?;

        let db = store::open(":memory:")?;
        let server = Server::with_logger(db.clone(), forwarder);
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let mut info = build_runtime_info("127.0.0.1:6667", &addr.to_string(), None);
        info.register_key = Some("owner-invite-secret".to_string());
        let logger = server.logger().clone();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router(server, info)).await;
        });

        let client = reqwest::Client::new();
        let base = format!("http://{addr}");

        // 1. Missing and invalid bearer tokens.
        assert_eq!(
            client.get(format!("{base}/v1/me")).send().await?.status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            client
                .get(format!("{base}/v1/me"))
                .bearer_auth("bogus-bearer-secret")
                .send()
                .await?
                .status(),
            StatusCode::UNAUTHORIZED
        );

        // 2. Registration denied and succeeded.
        assert_eq!(
            client
                .post(format!("{base}/v1/register"))
                .json(&json!({"nick": "audit-worker", "invite": "wrong-invite"}))
                .send()
                .await?
                .status(),
            StatusCode::FORBIDDEN
        );
        let reg_res = client
            .post(format!("{base}/v1/register"))
            .json(&json!({"nick": "audit-worker", "invite": "owner-invite-secret"}))
            .send()
            .await?;
        assert_eq!(reg_res.status(), StatusCode::CREATED);
        let worker_token = reg_res.json::<Value>().await?["token"]
            .as_str()
            .unwrap()
            .to_string();

        // 3. Owner reissue denied and succeeded.
        assert_eq!(
            client
                .post(format!("{base}/v1/owner/reissue"))
                .json(&json!({"nick": "audit-worker", "invite": "bad-owner-key"}))
                .send()
                .await?
                .status(),
            StatusCode::FORBIDDEN
        );
        let reissue_res = client
            .post(format!("{base}/v1/owner/reissue"))
            .json(&json!({"nick": "audit-worker", "invite": "owner-invite-secret"}))
            .send()
            .await?;
        assert_eq!(reissue_res.status(), StatusCode::OK);
        let reissued_token = reissue_res.json::<Value>().await?["token"]
            .as_str()
            .unwrap()
            .to_string();

        // 4. Role change.
        assert_eq!(
            client
                .post(format!("{base}/v1/roles"))
                .bearer_auth(&reissued_token)
                .json(&json!({"nick": "audit-worker", "role": "leader"}))
                .send()
                .await?
                .status(),
            StatusCode::OK
        );

        // 5. GitHub webhook signature failure.
        assert_eq!(
            client
                .post(format!("{base}/v1/gits"))
                .bearer_auth(&reissued_token)
                .json(&json!({"name": "audit-repo", "channel": "#general", "local_path": "/tmp/audit-repo", "webhook_secret": "gh-secret"}))
                .send()
                .await?
                .status(),
            StatusCode::CREATED
        );
        assert_eq!(
            client
                .post(format!("{base}/v1/git/webhook/audit-repo"))
                .header("X-Hub-Signature-256", "sha256=0000")
                .json(&json!({"ref": "refs/heads/main"}))
                .send()
                .await?
                .status(),
            StatusCode::UNAUTHORIZED
        );

        // 6. Trigger a real HTTP 500 internal server error by dropping a table.
        db.lock().unwrap().execute("DROP TABLE reports", [])?;
        assert_eq!(
            client
                .get(format!("{base}/v1/reports"))
                .bearer_auth(&reissued_token)
                .send()
                .await?
                .status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );

        logger.flush();
        let mut received = Vec::new();
        let mut buf = [0u8; 4096];
        while let Ok((n, _)) = syslog_rx.recv_from(&mut buf) {
            received.push(String::from_utf8_lossy(&buf[..n]).into_owned());
        }

        let joined = received.join("\n");
        for expected in [
            "auth.http_failed",
            "auth.register_denied",
            "auth.register_succeeded",
            "owner.auth_denied",
            "token.reissued",
            "role.changed",
            "git.webhook_denied",
            "http.internal_error",
        ] {
            assert!(
                received.iter().any(|msg| msg.contains(expected)),
                "missing syslog event '{expected}' in:\n{joined}"
            );
        }
        for secret in [
            "owner-invite-secret",
            "bogus-bearer-secret",
            &worker_token,
            &reissued_token,
            "gh-secret",
        ] {
            assert!(
                !joined.contains(secret),
                "syslog output leaked secret '{secret}'"
            );
        }
        assert!(
            received
                .iter()
                .any(|msg| msg.starts_with("<27>1 ") && msg.contains("http.internal_error")),
            "expected daemon error PRI <27> for http.internal_error"
        );

        Ok(())
    }

    #[tokio::test]
    async fn owner_backup_endpoints_enforce_owner_key_and_verify_snapshots() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("aircd-http-backup-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir)?;
        let db_path = dir.join("hub.sqlite3");
        let owner_key_path = dir.join("aircd-owner.key");
        let backup_dir = dir.join("backups");
        let owner_secret = "owner-backup-secret-xyz";
        std::fs::write(&owner_key_path, format!("{owner_secret}\n"))?;

        let db = store::open(&db_path)?;
        let backup_cfg = crate::backup::BackupConfig {
            enabled: true,
            db_path: db_path.display().to_string(),
            backup_dir: Some(backup_dir.clone()),
            interval_secs: 3600,
            retention: 3,
            on_startup: false,
            owner_key_path: Some(owner_key_path.clone()),
            tls_cert_path: None,
            tls_key_path: None,
            tls_client_ca_path: None,
        };
        let server = Server::with_logger_and_backup(
            db,
            crate::logging::EventForwarder::default(),
            backup_cfg,
        );
        let (_, worker_token) = server
            .auth()
            .register_principal("worker-b", Some("dev"), None)?;

        let mut info = build_runtime_info("127.0.0.1:0", "127.0.0.1:0", None);
        info.register_key = Some(owner_secret.to_string());

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router(server, info)).await;
        });

        let client = reqwest::Client::new();
        let base = format!("http://127.0.0.1:{port}");

        // Reject missing/wrong owner key and ordinary agent bearer tokens.
        assert_eq!(
            client
                .get(format!("{base}/v1/owner/backups"))
                .send()
                .await?
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            client
                .post(format!("{base}/v1/owner/backup"))
                .header("Authorization", format!("Bearer {worker_token}"))
                .json(&json!({ "action": "run" }))
                .send()
                .await?
                .status(),
            StatusCode::FORBIDDEN
        );

        // Trigger a verified backup with the owner key.
        let run_resp = client
            .post(format!("{base}/v1/owner/backup"))
            .header("X-AirCD-Invite", owner_secret)
            .json(&json!({ "action": "run" }))
            .send()
            .await?;
        assert_eq!(run_resp.status(), StatusCode::OK);
        let run_text = run_resp.text().await?;
        assert!(!run_text.contains(owner_secret));
        let run_json: serde_json::Value = serde_json::from_str(&run_text)?;
        assert_eq!(run_json["ok"], true);
        let snapshot_id = run_json["result"]["snapshot"]["snapshot_id"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(
            run_json["result"]["snapshot"]["integrity_check"],
            "ok"
        );
        assert_eq!(
            run_json["result"]["snapshot"]["key_ring"]["files"][0]["kind"],
            "owner_key"
        );

        // List and verify via owner endpoints.
        let list_json: serde_json::Value = client
            .get(format!("{base}/v1/owner/backups"))
            .header("X-AirCD-Invite", owner_secret)
            .send()
            .await?
            .json()
            .await?;
        assert_eq!(list_json["ok"], true);
        assert_eq!(list_json["snapshots"].as_array().unwrap().len(), 1);

        let verify_json: serde_json::Value = client
            .post(format!("{base}/v1/owner/backup"))
            .header("X-AirCD-Invite", owner_secret)
            .json(&json!({ "action": "verify", "snapshot_id": snapshot_id }))
            .send()
            .await?
            .json()
            .await?;
        assert_eq!(verify_json["ok"], true);
        assert_eq!(verify_json["verification"]["ok"], true);
        assert_eq!(verify_json["verification"]["key_ring_verified"], true);

        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    #[tokio::test]
    async fn owner_db_diagnostics_checkpoint_and_concurrent_burst_work() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("aircd-http-db-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir)?;
        let db_path = dir.join("hub.sqlite3");
        let sqlite_cfg = crate::db::SqlitePoolConfig {
            read_pool_size: 4,
            busy_timeout_ms: 4000,
            synchronous: crate::db::SqliteSynchronous::Normal,
            wal_autocheckpoint: 256,
            cache_size_kib: 4096,
            journal_size_limit_bytes: 16_777_216,
        };
        let db = store::open_with_config(&db_path, sqlite_cfg)?;
        let server = Server::new(db);
        let owner_secret = "owner-db-secret";

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let mut info = build_runtime_info("127.0.0.1:6667", &addr.to_string(), None);
        info.register_key = Some(owner_secret.to_string());
        let app = router(server.clone(), info);
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let client = reqwest::Client::new();
        let base = format!("http://{addr}");

        // Unauthenticated owner db calls are rejected.
        assert_eq!(
            client
                .get(format!("{base}/v1/owner/db"))
                .send()
                .await?
                .status(),
            StatusCode::FORBIDDEN
        );

        // Register several agents and fire concurrent reads + writes simulating a wake burst.
        let mut tokens = Vec::new();
        for i in 0..8 {
            let resp: serde_json::Value = client
                .post(format!("{base}/v1/register"))
                .header("X-AirCD-Invite", owner_secret)
                .json(&json!({ "nick": format!("burst-{i}") }))
                .send()
                .await?
                .json()
                .await?;
            assert_eq!(resp["ok"], true);
            tokens.push(resp["token"].as_str().unwrap().to_string());
        }

        let mut tasks = Vec::new();
        for (idx, token) in tokens.into_iter().enumerate() {
            let client = client.clone();
            let base = base.clone();
            tasks.push(tokio::spawn(async move {
                let post_resp = client
                    .post(format!("{base}/v1/messages"))
                    .bearer_auth(&token)
                    .json(&json!({ "channel": "#general", "body": format!("wake burst {idx}") }))
                    .send()
                    .await?;
                assert_eq!(post_resp.status(), StatusCode::CREATED);

                for _ in 0..3 {
                    let list_resp = client
                        .get(format!("{base}/v1/messages?channel=%23general&limit=20"))
                        .bearer_auth(&token)
                        .send()
                        .await?;
                    assert_eq!(list_resp.status(), StatusCode::OK);
                    let inbox_resp = client
                        .get(format!("{base}/v1/inbox"))
                        .bearer_auth(&token)
                        .send()
                        .await?;
                    assert_eq!(inbox_resp.status(), StatusCode::OK);
                }
                anyhow::Ok(())
            }));
        }
        for task in tasks {
            task.await??;
        }

        let db_json: serde_json::Value = client
            .get(format!("{base}/v1/owner/db"))
            .header("X-AirCD-Invite", owner_secret)
            .send()
            .await?
            .json()
            .await?;
        assert_eq!(db_json["ok"], true);
        assert_eq!(db_json["diagnostics"]["pragmas"]["journal_mode"], "wal");
        assert_eq!(db_json["diagnostics"]["pragmas"]["synchronous"], "normal");
        assert_eq!(db_json["diagnostics"]["pragmas"]["busy_timeout_ms"], 4000);
        assert_eq!(db_json["diagnostics"]["pragmas"]["reader_query_only"], true);
        assert!(
            db_json["diagnostics"]["pool"]["read_acquires"]
                .as_u64()
                .unwrap()
                >= 48
        );
        assert!(
            db_json["diagnostics"]["pool"]["write_acquires"]
                .as_u64()
                .unwrap()
                >= 8
        );

        let cp_json: serde_json::Value = client
            .post(format!("{base}/v1/owner/db/checkpoint"))
            .header("X-AirCD-Invite", owner_secret)
            .json(&json!({ "mode": "truncate" }))
            .send()
            .await?
            .json()
            .await?;
        assert_eq!(cp_json["ok"], true);
        assert_eq!(cp_json["checkpoint"]["mode"], "truncate");
        assert_eq!(cp_json["checkpoint"]["busy"], 0);

        handle.abort();
        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    #[tokio::test]
    async fn http_endpoints_enforce_token_scopes_ttl_and_refresh() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("aircd-http-scopes-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir)?;
        let db = store::open(dir.join("hub.sqlite3"))?;
        let server = Server::with_logger_backup_and_ttl(
            db.clone(),
            crate::logging::EventForwarder::default(),
            crate::backup::BackupConfig::default(),
            Some(3600),
        );
        let owner_secret = "owner-scopes-secret";

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let mut info = build_runtime_info("127.0.0.1:6667", &addr.to_string(), None);
        info.register_key = Some(owner_secret.to_string());
        let app = router(server.clone(), info);
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let client = reqwest::Client::new();
        let base = format!("http://{addr}");

        // Register a leader principal; initial token defaults to ["admin"] and inherits default TTL.
        let reg_resp = client
            .post(format!("{base}/v1/register"))
            .header("X-AirCD-Invite", owner_secret)
            .json(&json!({ "nick": "scope-lead", "role": "leader" }))
            .send()
            .await?;
        assert_eq!(reg_resp.status(), StatusCode::CREATED);
        let reg_json: serde_json::Value = reg_resp.json().await?;
        let admin_token = reg_json["token"].as_str().unwrap().to_string();
        assert_eq!(reg_json["scopes"], json!(["admin"]));
        assert_eq!(reg_json["ttl_seconds"], json!(3600));
        assert!(reg_json["expires_at"].as_i64().is_some());

        // Owner-only endpoints remain owner-key guarded even against admin bearer tokens.
        assert_eq!(
            client
                .get(format!("{base}/v1/owner/db"))
                .bearer_auth(&admin_token)
                .send()
                .await?
                .status(),
            StatusCode::FORBIDDEN
        );

        // Register a worker principal and issue read-only, builder, and pm scoped tokens.
        let worker_reg: serde_json::Value = client
            .post(format!("{base}/v1/register"))
            .header("X-AirCD-Invite", owner_secret)
            .json(&json!({ "nick": "scope-worker", "role": "builder" }))
            .send()
            .await?
            .json()
            .await?;
        let worker_admin_token = worker_reg["token"].as_str().unwrap().to_string();

        let issue = |nick: &str, scopes: serde_json::Value, ttl: Option<u64>, label: &str| {
            let client = client.clone();
            let base = base.clone();
            let admin_token = admin_token.clone();
            let nick = nick.to_string();
            let label = label.to_string();
            async move {
                let mut body = json!({
                    "nick": nick,
                    "label": label,
                    "scopes": scopes,
                });
                if let Some(ttl) = ttl {
                    body["ttl_seconds"] = json!(ttl);
                }
                client
                    .post(format!("{base}/v1/tokens"))
                    .bearer_auth(&admin_token)
                    .json(&body)
                    .send()
                    .await
            }
        };

        let ro_json: serde_json::Value = issue("scope-worker", json!(["read-only"]), Some(600), "ro")
            .await?
            .json()
            .await?;
        assert_eq!(ro_json["scopes"], json!(["read"]));
        let ro_token = ro_json["token"].as_str().unwrap().to_string();
        let ro_token_id = ro_json["token_id"].as_str().unwrap().to_string();

        let rw_json: serde_json::Value = issue("scope-worker", json!(["builder"]), Some(600), "rw")
            .await?
            .json()
            .await?;
        assert_eq!(rw_json["scopes"], json!(["write"]));
        let rw_token = rw_json["token"].as_str().unwrap().to_string();

        let pm_json: serde_json::Value = issue("scope-lead", json!(["pm"]), Some(600), "pm")
            .await?
            .json()
            .await?;
        let pm_token = pm_json["token"].as_str().unwrap().to_string();

        // Create a project using admin token (required by POST /v1/projects).
        assert_eq!(
            client
                .post(format!("{base}/v1/projects"))
                .bearer_auth(&pm_token)
                .json(&json!({ "channel": "#scopeproj", "name": "Scope Proj" }))
                .send()
                .await?
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            client
                .post(format!("{base}/v1/projects"))
                .bearer_auth(&admin_token)
                .json(&json!({ "channel": "#scopeproj", "name": "Scope Proj" }))
                .send()
                .await?
                .status(),
            StatusCode::CREATED
        );
        assert_eq!(
            client
                .post(format!("{base}/v1/projects/members"))
                .bearer_auth(&pm_token)
                .json(&json!({ "channel": "#scopeproj", "nick": "scope-worker", "role": "builder" }))
                .send()
                .await?
                .status(),
            StatusCode::OK
        );

        // read-only allows GETs (including /v1/me with token metadata) and denies writes.
        let me_json: serde_json::Value = client
            .get(format!("{base}/v1/me"))
            .bearer_auth(&ro_token)
            .send()
            .await?
            .json()
            .await?;
        assert_eq!(me_json["token"]["scopes"], json!(["read"]));
        assert_eq!(
            client
                .post(format!("{base}/v1/messages"))
                .bearer_auth(&ro_token)
                .json(&json!({ "channel": "#scopeproj", "body": "no" }))
                .send()
                .await?
                .status(),
            StatusCode::FORBIDDEN
        );

        // builder (write) allows messages and claim/testing/review transitions, denies task create/assign/merge & roles.
        assert_eq!(
            client
                .post(format!("{base}/v1/messages"))
                .bearer_auth(&rw_token)
                .json(&json!({ "channel": "#scopeproj", "body": "hello" }))
                .send()
                .await?
                .status(),
            StatusCode::CREATED
        );
        assert_eq!(
            client
                .post(format!("{base}/v1/tasks"))
                .bearer_auth(&rw_token)
                .json(&json!({ "channel": "#scopeproj", "title": "Denied" }))
                .send()
                .await?
                .status(),
            StatusCode::FORBIDDEN
        );

        // pm creates tasks and assigns one, builder claims an open task and moves assigned -> testing -> review, pm merges; builder cannot merge or set roles.
        let claimable_task: serde_json::Value = client
            .post(format!("{base}/v1/tasks"))
            .bearer_auth(&pm_token)
            .json(&json!({ "channel": "#scopeproj", "title": "Claimable task" }))
            .send()
            .await?
            .json()
            .await?;
        let claim_id = claimable_task["task"]["id"].as_str().unwrap().to_string();
        assert_eq!(
            client
                .post(format!("{base}/v1/tasks/{claim_id}/claim"))
                .bearer_auth(&rw_token)
                .send()
                .await?
                .status(),
            StatusCode::OK
        );

        let created_task: serde_json::Value = client
            .post(format!("{base}/v1/tasks"))
            .bearer_auth(&pm_token)
            .json(&json!({ "channel": "#scopeproj", "title": "Scoped task" }))
            .send()
            .await?
            .json()
            .await?;
        let task_id = created_task["task"]["id"].as_str().unwrap().to_string();
        assert_eq!(
            client
                .post(format!("{base}/v1/tasks/{task_id}/assign"))
                .bearer_auth(&rw_token)
                .json(&json!({ "assignee": "scope-worker" }))
                .send()
                .await?
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            client
                .post(format!("{base}/v1/tasks/{task_id}/assign"))
                .bearer_auth(&pm_token)
                .json(&json!({ "assignee": "scope-worker" }))
                .send()
                .await?
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            client
                .post(format!("{base}/v1/tasks/{task_id}/status"))
                .bearer_auth(&rw_token)
                .json(&json!({ "status": "testing" }))
                .send()
                .await?
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            client
                .post(format!("{base}/v1/tasks/{task_id}/status"))
                .bearer_auth(&rw_token)
                .json(&json!({ "status": "review", "note": "all unit tests pass" }))
                .send()
                .await?
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            client
                .post(format!("{base}/v1/tasks/{task_id}/status"))
                .bearer_auth(&rw_token)
                .json(&json!({ "status": "merged" }))
                .send()
                .await?
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            client
                .post(format!("{base}/v1/tasks/{task_id}/status"))
                .bearer_auth(&pm_token)
                .json(&json!({ "status": "merged" }))
                .send()
                .await?
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            client
                .post(format!("{base}/v1/roles"))
                .bearer_auth(&pm_token)
                .json(&json!({ "nick": "scope-worker", "role": "reviewer" }))
                .send()
                .await?
                .status(),
            StatusCode::FORBIDDEN
        );

        // admin can set roles, list another user's tokens, and force-unlock.
        assert_eq!(
            client
                .post(format!("{base}/v1/roles"))
                .bearer_auth(&admin_token)
                .json(&json!({ "nick": "scope-worker", "role": "reviewer" }))
                .send()
                .await?
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            client
                .get(format!("{base}/v1/tokens?nick=scope-worker"))
                .bearer_auth(&pm_token)
                .send()
                .await?
                .status(),
            StatusCode::FORBIDDEN
        );
        let worker_tokens: serde_json::Value = client
            .get(format!("{base}/v1/tokens?nick=scope-worker"))
            .bearer_auth(&admin_token)
            .send()
            .await?
            .json()
            .await?;
        assert!(worker_tokens["tokens"].as_array().unwrap().len() >= 2);

        // POST /v1/tokens/refresh extends TTL and keeps existing scopes without escalation.
        let refresh_json: serde_json::Value = client
            .post(format!("{base}/v1/tokens/refresh"))
            .bearer_auth(&ro_token)
            .json(&json!({ "ttl_seconds": 7200 }))
            .send()
            .await?
            .json()
            .await?;
        assert_eq!(refresh_json["scopes"], json!(["read"]));
        assert_eq!(refresh_json["ttl_seconds"], json!(7200));
        assert_eq!(refresh_json["token"], json!(ro_token));

        // POST /v1/login with rotate_token preserves scopes too.
        let login_json: serde_json::Value = client
            .post(format!("{base}/v1/login"))
            .bearer_auth(&ro_token)
            .json(&json!({ "rotate_token": true }))
            .send()
            .await?
            .json()
            .await?;
        let login_rotated = login_json["rotated_token"].as_str().unwrap().to_string();
        assert_ne!(login_rotated, ro_token);
        assert_eq!(login_json["scopes"], json!(["read"]));

        // POST /v1/tokens/refresh with rotate=true replaces the caller's token and retires the old secret.
        let rotated_resp: serde_json::Value = client
            .post(format!("{base}/v1/tokens/refresh"))
            .bearer_auth(&ro_token)
            .json(&json!({ "rotate": true }))
            .send()
            .await?
            .json()
            .await?;
        let rotated_ro = rotated_resp["token"].as_str().unwrap().to_string();
        assert_ne!(rotated_ro, ro_token);
        assert_eq!(rotated_resp["scopes"], json!(["read"]));
        // Old rotated token is now rejected with 401.
        assert_eq!(
            client
                .get(format!("{base}/v1/me"))
                .bearer_auth(&ro_token)
                .send()
                .await?
                .status(),
            StatusCode::UNAUTHORIZED
        );
        // Also verify worker_admin_token can acquire & another admin force-unlock.
        assert_eq!(
            client
                .post(format!("{base}/v1/locks"))
                .bearer_auth(&worker_admin_token)
                .json(&json!({ "resource": "res-lock", "channel": "#scopeproj" }))
                .send()
                .await?
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            client
                .post(format!("{base}/v1/locks/release"))
                .bearer_auth(&pm_token)
                .json(&json!({ "resource": "res-lock", "force": true }))
                .send()
                .await?
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            client
                .post(format!("{base}/v1/locks/release"))
                .bearer_auth(&admin_token)
                .json(&json!({ "resource": "res-lock", "force": true }))
                .send()
                .await?
                .status(),
            StatusCode::OK
        );

        // Expired token returns 401 Unauthorized.
        let _ = ro_token_id;
        let rotated_ro_id = rotated_resp["token_id"].as_str().unwrap();
        db.write()?.execute(
            "UPDATE principal_tokens SET expires_at = 1 WHERE token_id = ?1",
            rusqlite::params![rotated_ro_id],
        )?;
        assert_eq!(
            client
                .get(format!("{base}/v1/me"))
                .bearer_auth(&rotated_ro)
                .send()
                .await?
                .status(),
            StatusCode::UNAUTHORIZED
        );

        handle.abort();
        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }
}
