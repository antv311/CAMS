fn scrape_metric_value<'a>(body: &'a str, line_prefix: &str) -> Option<&'a str> {
    body.lines().find_map(|line| {
        if line.starts_with('#') {
            return None;
        }
        let rest = line.strip_prefix(line_prefix)?;
        // Exact metric match: followed by whitespace and numeric value.
        if rest.starts_with(' ') {
            Some(rest.trim())
        } else {
            None
        }
    })
}

fn scrape_metric_u64(body: &str, line_prefix: &str) -> u64 {
    scrape_metric_value(body, line_prefix)
        .unwrap_or_else(|| panic!("missing metric '{line_prefix}' in:\n{body}"))
        .parse::<u64>()
        .unwrap_or_else(|e| panic!("invalid u64 for '{line_prefix}': {e}"))
}

fn scrape_metric_f64(body: &str, line_prefix: &str) -> f64 {
    scrape_metric_value(body, line_prefix)
        .unwrap_or_else(|| panic!("missing metric '{line_prefix}' in:\n{body}"))
        .parse::<f64>()
        .unwrap_or_else(|e| panic!("invalid f64 for '{line_prefix}': {e}"))
}

fn shell_wake_input(script: &str, cooldown_seconds: i64, timeout_seconds: i64) -> crate::wake::WakeConfigInput {
    let (command, flag) = if cfg!(windows) {
        ("cmd", "/C")
    } else {
        ("sh", "-c")
    };
    crate::wake::WakeConfigInput {
        command: command.to_string(),
        args: vec![flag.to_string(), script.to_string()],
        enabled: Some(true),
        cooldown_seconds: Some(cooldown_seconds),
        timeout_seconds: Some(timeout_seconds),
        ..Default::default()
    }
}

#[tokio::test]
async fn prometheus_metrics_format_active_sessions_and_rate_limits() -> Result<()> {
    let (server, base, http_task) = start_rate_limited_http(
        crate::rate_limit::RateLimitConfig {
            enabled: true,
            ip_requests_per_sec: 100.0,
            ip_burst: 50,
            principal_requests_per_sec: 0.5,
            principal_burst: 1,
            trust_proxy_headers: false,
            max_buckets: 100,
        },
        None,
    )
    .await?;

    let irc_listener = TcpListener::bind("127.0.0.1:0").await?;
    let irc_addr = irc_listener.local_addr()?;
    let irc_server = server.clone();
    let irc_task = tokio::spawn(async move {
        let _ = irc_server.listen_on(irc_listener).await;
    });

    let (_, token) = server
        .auth()
        .register_principal("prom-agent", Some("member"), None)?;

    // Connect an IRC client session.
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let stream_1 = tokio::net::TcpStream::connect(irc_addr).await?;
    let (r1, mut w1) = stream_1.into_split();
    let mut lines_1 = BufReader::new(r1).lines();
    w1.write_all(format!("PASS {token}\r\nNICK prom-agent\r\nUSER prom-agent 0 * :test\r\n").as_bytes())
        .await?;
    while let Some(line) = lines_1.next_line().await? {
        if line.contains(" 001 ") {
            break;
        }
    }

    // Trigger one principal rate-limit rejection (burst=1 -> 2nd request returns 429).
    let client = reqwest::Client::new();
    let ok_resp = client
        .get(format!("{base}/v1/me"))
        .bearer_auth(&token)
        .send()
        .await?;
    assert_eq!(ok_resp.status(), StatusCode::OK);
    let limited_resp = client
        .get(format!("{base}/v1/me"))
        .bearer_auth(&token)
        .send()
        .await?;
    assert_eq!(limited_resp.status(), StatusCode::TOO_MANY_REQUESTS);

    // Scrape /metrics while IRC session is active.
    let resp = client.get(format!("{base}/metrics")).send().await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some(crate::metrics::PROMETHEUS_CONTENT_TYPE)
    );
    let body = resp.text().await?;
    assert!(!body.contains("\r\n"));
    assert_eq!(scrape_metric_u64(&body, "aircd_up"), 1);
    assert!(scrape_metric_f64(&body, "aircd_uptime_seconds") >= 0.0);
    assert_eq!(
        scrape_metric_u64(&body, "aircd_active_sessions{kind=\"irc\"}"),
        1
    );
    assert_eq!(
        scrape_metric_u64(&body, "aircd_irc_sessions_total{event=\"connected\"}"),
        1
    );
    assert_eq!(
        scrape_metric_u64(&body, "aircd_http_rate_limited_total{scope=\"principal\"}"),
        1
    );
    assert!(scrape_metric_u64(&body, "aircd_http_requests_total{method=\"GET\",status_class=\"2xx\"}") >= 1);
    assert_eq!(
        scrape_metric_u64(&body, "aircd_http_requests_total{method=\"GET\",status_class=\"4xx\"}"),
        1
    );
    assert!(scrape_metric_u64(&body, "aircd_http_request_duration_seconds_count") >= 2);
    assert!(scrape_metric_u64(&body, "aircd_principals_total") >= 1);

    // Replace the session with a second IRC connection, then disconnect it.
    let stream_2 = tokio::net::TcpStream::connect(irc_addr).await?;
    let (r2, mut w2) = stream_2.into_split();
    let mut lines_2 = BufReader::new(r2).lines();
    w2.write_all(format!("PASS {token}\r\nNICK prom-agent\r\nUSER prom-agent 0 * :test2\r\n").as_bytes())
        .await?;
    while let Some(line) = lines_2.next_line().await? {
        if line.contains(" 001 ") {
            break;
        }
    }
    drop(w1);
    drop(lines_1);
    w2.write_all(b"QUIT :bye\r\n").await?;
    drop(w2);
    drop(lines_2);

    // Wait briefly for server disconnect cleanup.
    for _ in 0..40 {
        let body = client.get(format!("{base}/metrics")).send().await?.text().await?;
        if scrape_metric_u64(&body, "aircd_active_sessions{kind=\"irc\"}") == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }

    let after = client.get(format!("{base}/metrics")).send().await?.text().await?;
    assert_eq!(
        scrape_metric_u64(&after, "aircd_active_sessions{kind=\"irc\"}"),
        0
    );
    assert_eq!(
        scrape_metric_u64(&after, "aircd_irc_sessions_total{event=\"connected\"}"),
        2
    );
    assert_eq!(
        scrape_metric_u64(&after, "aircd_irc_sessions_total{event=\"replaced\"}"),
        1
    );
    assert!(scrape_metric_u64(&after, "aircd_irc_sessions_total{event=\"disconnected\"}") >= 1);

    irc_task.abort();
    http_task.abort();
    Ok(())
}

#[tokio::test]
async fn prometheus_metrics_track_task_throughput_and_wake_latencies() -> Result<()> {
    let api = TemplateApi::start(Some("metrics-owner-key")).await?;
    let (pm, _) = api
        .server
        .auth()
        .register_principal("pm", Some("leader"), None)?;
    let (worker, _) = api
        .server
        .auth()
        .register_principal("worker", Some("member"), None)?;
    api.server.projects().create("#proj", "Proj", None, &pm.id)?;
    api.server
        .projects()
        .set_role("#proj", &pm.id, "leader")?;
    api.server
        .projects()
        .set_role("#proj", &worker.id, "member")?;
    api.server.projects().set_pm("#proj", Some(&pm.id))?;

    // 1. Task throughput: lease flow (open -> claimed -> done) and review flow (assigned -> in_progress -> blocked -> in_progress -> testing -> review -> merged).
    let lease_task = api.server.tasks().create_full(crate::tasks::NewTask {
        channel: "#proj".to_string(),
        title: "Lease task".to_string(),
        kind: Some("bug".to_string()),
        ..Default::default()
    })?;
    api.server.tasks().claim(&lease_task.id, &worker.id)?;
    api.server.tasks().done(&lease_task.id, &worker.id)?;

    let feature_task = api.server.tasks().create_full(crate::tasks::NewTask {
        channel: "#proj".to_string(),
        title: "Feature task".to_string(),
        kind: Some("feature".to_string()),
        assignee: Some(worker.id.clone()),
        ..Default::default()
    })?;
    api.server.set_task_status_api(&worker, &feature_task.id, "in_progress", None, None, None, None)?;
    api.server.tasks().ask(&feature_task.id, &worker.id, "Need decision?")?;
    api.server.tasks().answer(&feature_task.id, &pm.id, "Proceed")?;
    api.server.set_task_status_api(&worker, &feature_task.id, "testing", None, None, None, None)?;
    api.server.set_task_status_api(
        &worker,
        &feature_task.id,
        "review",
        Some("All tests pass"),
        Some("feat/test"),
        None,
        None,
    )?;
    api.server.set_task_status_api(&pm, &feature_task.id, "merged", None, None, None, None)?;

    // 2. Wake latencies & active/stored wake sessions:
    // Run one slow-ish wake + queue a follow-up wake, then a failing wake.
    let ok_script = if cfg!(windows) {
        "ping -n 1 127.0.0.1 >nul & echo session_id=wake_sess_1"
    } else {
        "sleep 0.05; echo session_id=wake_sess_1"
    };
    let mut wake_cfg = shell_wake_input(ok_script, 0, 10);
    wake_cfg.session_mode = Some("resume".to_string());
    wake_cfg.session_marker = Some("session_id=".to_string());
    api.server.wakes().upsert(&worker.id, wake_cfg)?;

    let ctx = crate::wake::WakeContext {
        principal_id: worker.id.clone(),
        nick: worker.nick.clone(),
        reason: "task assigned".to_string(),
        channel: "#proj".to_string(),
        task_id: Some(feature_task.id.clone()),
        ..Default::default()
    };
    assert!(api.server.wakes().request(ctx.clone())?);
    assert!(api.server.wakes().request(ctx.clone())?); // queued while first runs
    assert!(api.server.wakes().request(ctx)?); // coalesced into pending slot

    // Wait for both ok wakes to finish.
    for _ in 0..80 {
        let runs = api.server.wakes().runs(Some(&worker.id), 10)?;
        if runs.len() >= 2 && runs.iter().all(|r| r.status != "running") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // Trigger one failing wake.
    let fail_cfg = shell_wake_input("exit 1", 0, 10);
    api.server.wakes().upsert(&pm.id, fail_cfg)?;
    assert!(api.server.wakes().request(crate::wake::WakeContext {
        principal_id: pm.id.clone(),
        nick: pm.nick.clone(),
        reason: "review needed".to_string(),
        channel: "#proj".to_string(),
        ..Default::default()
    })?);
    for _ in 0..80 {
        let runs = api.server.wakes().runs(Some(&pm.id), 10)?;
        if runs.len() == 1 && runs[0].status == "failed" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    let resp = api
        .client
        .get(format!("{}/metrics", api.base))
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.text().await?;

    // Verify task throughput metrics.
    assert_eq!(
        scrape_metric_u64(&body, "aircd_tasks_created_total{kind=\"bug\"}"),
        1
    );
    assert_eq!(
        scrape_metric_u64(&body, "aircd_tasks_created_total{kind=\"feature\"}"),
        1
    );
    assert_eq!(
        scrape_metric_u64(&body, "aircd_tasks_completed_total{outcome=\"done\"}"),
        1
    );
    assert_eq!(
        scrape_metric_u64(&body, "aircd_tasks_completed_total{outcome=\"merged\"}"),
        1
    );
    assert_eq!(
        scrape_metric_u64(&body, "aircd_tasks_current{status=\"done\"}"),
        1
    );
    assert_eq!(
        scrape_metric_u64(&body, "aircd_tasks_current{status=\"merged\"}"),
        1
    );
    assert_eq!(
        scrape_metric_u64(
            &body,
            "aircd_task_transitions_total{from=\"open\",to=\"claimed\"}"
        ),
        1
    );
    assert_eq!(
        scrape_metric_u64(
            &body,
            "aircd_task_transitions_total{from=\"claimed\",to=\"done\"}"
        ),
        1
    );
    assert_eq!(
        scrape_metric_u64(
            &body,
            "aircd_task_transitions_total{from=\"review\",to=\"merged\"}"
        ),
        1
    );
    assert_eq!(
        scrape_metric_u64(&body, "aircd_task_questions_total{action=\"asked\"}"),
        1
    );
    assert_eq!(
        scrape_metric_u64(&body, "aircd_task_questions_total{action=\"answered\"}"),
        1
    );
    assert!(
        scrape_metric_u64(&body, "aircd_task_events_persisted_total{action=\"status\"}") >= 4
    );

    // Verify wake session & latency metrics.
    assert_eq!(
        scrape_metric_u64(&body, "aircd_active_sessions{kind=\"wake_stored\"}"),
        1
    );
    assert_eq!(
        scrape_metric_u64(&body, "aircd_wake_requests_total{result=\"started\"}"),
        2
    );
    assert_eq!(
        scrape_metric_u64(&body, "aircd_wake_requests_total{result=\"queued\"}"),
        1
    );
    assert_eq!(
        scrape_metric_u64(&body, "aircd_wake_requests_total{result=\"coalesced\"}"),
        1
    );
    assert_eq!(
        scrape_metric_u64(&body, "aircd_wake_runs_total{status=\"ok\"}"),
        2
    );
    assert_eq!(
        scrape_metric_u64(&body, "aircd_wake_runs_total{status=\"failed\"}"),
        1
    );
    assert_eq!(
        scrape_metric_u64(&body, "aircd_wake_dispatch_duration_seconds_count"),
        3
    );
    assert!(scrape_metric_f64(&body, "aircd_wake_dispatch_duration_seconds_sum") > 0.0);
    assert_eq!(
        scrape_metric_u64(
            &body,
            "aircd_wake_duration_seconds_count{outcome=\"ok\"}"
        ),
        2
    );
    assert_eq!(
        scrape_metric_u64(
            &body,
            "aircd_wake_duration_seconds_count{outcome=\"failed\"}"
        ),
        1
    );
    assert_eq!(
        scrape_metric_u64(
            &body,
            "aircd_wake_end_to_end_duration_seconds_count{outcome=\"ok\"}"
        ),
        2
    );
    assert!(
        scrape_metric_f64(
            &body,
            "aircd_wake_end_to_end_duration_seconds_sum{outcome=\"ok\"}"
        ) >= scrape_metric_f64(&body, "aircd_wake_duration_seconds_sum{outcome=\"ok\"}")
    );

    Ok(())
}

#[tokio::test]
async fn prometheus_metrics_live_wake_slots_timeout_quota_skip_and_strict_format() -> Result<()> {
    let api = TemplateApi::start(None).await?;
    let (worker, _) = api
        .server
        .auth()
        .register_principal("slow-worker", Some("member"), None)?;
    let (quota_worker, _) = api
        .server
        .auth()
        .register_principal("quota-worker", Some("member"), None)?;

    // Slow wake (1s sleep) with cooldown=0, timeout=5 so we can observe wake_running=1 & wake_pending=1 live.
    let slow_script = if cfg!(windows) {
        "ping -n 2 127.0.0.1 >nul"
    } else {
        "sleep 0.5"
    };
    api.server
        .wakes()
        .upsert(&worker.id, shell_wake_input(slow_script, 0, 5))?;

    let ctx = crate::wake::WakeContext {
        principal_id: worker.id.clone(),
        nick: worker.nick.clone(),
        reason: "live slot check".to_string(),
        channel: "#general".to_string(),
        ..Default::default()
    };
    assert!(api.server.wakes().request(ctx.clone())?);
    assert!(api.server.wakes().request(ctx)?);

    // Wait until the first run enters 'running' in SQLite (meaning ActiveWakeGuard is active).
    for _ in 0..40 {
        let runs = api.server.wakes().runs(Some(&worker.id), 10)?;
        if runs.iter().any(|r| r.status == "running") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    let live_body = api
        .client
        .get(format!("{}/metrics", api.base))
        .send()
        .await?
        .text()
        .await?;
    assert_eq!(
        scrape_metric_u64(&live_body, "aircd_active_sessions{kind=\"wake_running\"}"),
        1
    );
    assert_eq!(
        scrape_metric_u64(&live_body, "aircd_active_sessions{kind=\"wake_pending\"}"),
        1
    );

    // Wait for both slow runs to complete.
    for _ in 0..100 {
        let runs = api.server.wakes().runs(Some(&worker.id), 10)?;
        if runs.len() >= 2 && runs.iter().all(|r| r.status != "running") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // Timeout wake (timeout_seconds = 1, command sleeps ~5s).
    let timeout_script = if cfg!(windows) {
        "ping -n 6 127.0.0.1 >nul"
    } else {
        "sleep 5"
    };
    api.server
        .wakes()
        .upsert(&worker.id, shell_wake_input(timeout_script, 0, 1))?;
    assert!(api.server.wakes().request(crate::wake::WakeContext {
        principal_id: worker.id.clone(),
        nick: worker.nick.clone(),
        reason: "timeout check".to_string(),
        channel: "#general".to_string(),
        ..Default::default()
    })?);
    for _ in 0..80 {
        let runs = api.server.wakes().runs(Some(&worker.id), 10)?;
        if runs.iter().any(|r| r.status == "timeout") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // Quota failure sets quota cooldown and causes subsequent request to be skipped.
    let quota_script = if cfg!(windows) {
        "echo resource_exhausted 1>&2 & exit 1"
    } else {
        "echo resource_exhausted >&2; exit 1"
    };
    api.server
        .wakes()
        .upsert(&quota_worker.id, shell_wake_input(quota_script, 0, 10))?;
    let quota_ctx = crate::wake::WakeContext {
        principal_id: quota_worker.id.clone(),
        nick: quota_worker.nick.clone(),
        reason: "quota check".to_string(),
        channel: "#general".to_string(),
        ..Default::default()
    };
    assert!(api.server.wakes().request(quota_ctx.clone())?);
    for _ in 0..80 {
        let runs = api.server.wakes().runs(Some(&quota_worker.id), 10)?;
        if runs.len() == 1 && runs[0].status == "failed" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(!api.server.wakes().request(quota_ctx)?);

    let final_body = api
        .client
        .get(format!("{}/metrics", api.base))
        .send()
        .await?
        .text()
        .await?;
    assert_eq!(
        scrape_metric_u64(&final_body, "aircd_active_sessions{kind=\"wake_running\"}"),
        0
    );
    assert_eq!(
        scrape_metric_u64(&final_body, "aircd_active_sessions{kind=\"wake_pending\"}"),
        0
    );
    assert_eq!(
        scrape_metric_u64(&final_body, "aircd_wake_runs_total{status=\"timeout\"}"),
        1
    );
    assert_eq!(
        scrape_metric_u64(
            &final_body,
            "aircd_wake_duration_seconds_count{outcome=\"timeout\"}"
        ),
        1
    );
    assert_eq!(
        scrape_metric_u64(&final_body, "aircd_wake_quota_cooldowns_active"),
        1
    );
    assert_eq!(
        scrape_metric_u64(&final_body, "aircd_wake_requests_total{result=\"skipped\"}"),
        1
    );
    assert_eq!(
        scrape_metric_u64(&final_body, "aircd_wake_runs_total{status=\"skipped\"}"),
        1
    );

    // Structural Prometheus 0.0.4 validation across every emitted line.
    let mut seen_help = std::collections::HashSet::new();
    let mut seen_type = std::collections::HashSet::new();
    for line in final_body.lines() {
        assert!(!line.is_empty(), "unexpected blank line in Prometheus output");
        if let Some(rest) = line.strip_prefix("# HELP ") {
            let name = rest.split_whitespace().next().unwrap();
            assert!(seen_help.insert(name.to_string()), "duplicate HELP for {name}");
        } else if let Some(rest) = line.strip_prefix("# TYPE ") {
            let mut parts = rest.split_whitespace();
            let name = parts.next().unwrap();
            let kind = parts.next().unwrap();
            assert!(matches!(kind, "counter" | "gauge" | "histogram"));
            assert!(seen_type.insert(name.to_string()), "duplicate TYPE for {name}");
        } else {
            assert!(!line.starts_with('#'), "unexpected comment line: {line}");
            let (metric_part, value_part) = line
                .rsplit_once(' ')
                .unwrap_or_else(|| panic!("missing value separator in: {line}"));
            assert!(!metric_part.is_empty());
            let parsed: f64 = value_part
                .parse()
                .unwrap_or_else(|e| panic!("invalid numeric value '{value_part}' in '{line}': {e}"));
            assert!(parsed.is_finite(), "non-finite metric value in: {line}");
        }
    }
    assert_eq!(seen_help, seen_type);

    Ok(())
}

#[tokio::test]
async fn owner_drain_endpoint_and_metrics_track_shutdown_state() -> Result<()> {
    let db = crate::store::open(":memory:")?;
    let server = crate::server::Server::new(db).with_shutdown_config(crate::shutdown::ShutdownConfig {
        drain_timeout_secs: 0,
        http_drain_timeout_secs: 1,
        checkpoint_leases_on_drain: true,
        lease_checkpoint_extension_secs: 300,
    })?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let base = format!("http://{addr}");
    let mut info = build_runtime_info("127.0.0.1:6667", &addr.to_string(), None);
    info.http_port = addr.port();
    info.register_key = Some("drain-owner-key".into());
    let http_server = server.clone();
    let stop_rx = server.shutdown().stop_accepting_rx();
    let http_task = tokio::spawn(async move {
        let _ = crate::http::serve_on_with_shutdown(listener, http_server, info, stop_rx).await;
    });

    let client = reqwest::Client::new();
    let (worker, _) = server
        .auth()
        .register_principal("api-drain-worker", Some("member"), None)?;
    server
        .projects()
        .create("#proj", "Proj", None, &worker.id)?;
    let task = server.tasks().create("#proj", "Drain task")?;
    let _ = server.tasks().claim(&task.id, &worker.id)?;

    // Hold worker slot running so a queued wake exists when drain starts.
    let script = if cfg!(windows) {
        "ping -n 3 127.0.0.1 >nul"
    } else {
        "sleep 2"
    };
    server
        .wakes()
        .upsert(&worker.id, shell_wake_input(script, 0, 10))?;
    let wake_ctx = crate::wake::WakeContext {
        principal_id: worker.id.clone(),
        nick: worker.nick.clone(),
        reason: "initial wake".into(),
        channel: "#proj".into(),
        task_id: Some(task.id.clone()),
        ..Default::default()
    };
    assert!(server.wakes().request(wake_ctx.clone())?);
    for _ in 0..60 {
        if server.wakes().active_execution_count() == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(server.wakes().active_execution_count(), 1);
    assert!(server.wakes().request(wake_ctx)?); // queued in pending slot

    // 1. /healthz keeps service=aircd, ok=true and reports draining=false before drain.
    let hz: Value = client
        .get(format!("{base}/healthz"))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(hz["ok"], true);
    assert_eq!(hz["service"], "aircd");
    assert_eq!(hz["draining"], false);
    assert_eq!(hz["phase"], "running");
    assert_eq!(hz["active_wake_executions"], 1);

    // 2. Non-owner cannot query or trigger drain.
    let (_, non_owner_tok) = server
        .auth()
        .register_principal("ordinary", Some("member"), None)?;
    let unauth_get = client
        .get(format!("{base}/v1/owner/drain"))
        .bearer_auth(&non_owner_tok)
        .send()
        .await?;
    assert_eq!(unauth_get.status(), StatusCode::FORBIDDEN);
    let unauth_post = client
        .post(format!("{base}/v1/owner/drain"))
        .bearer_auth(&non_owner_tok)
        .send()
        .await?;
    assert_eq!(unauth_post.status(), StatusCode::FORBIDDEN);

    let status_before: Value = client
        .get(format!("{base}/v1/owner/drain"))
        .header("X-AirCD-Invite", "drain-owner-key")
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(status_before["ok"], true);
    assert_eq!(status_before["draining"], false);
    assert_eq!(status_before["phase"], "running");

    // 3. POST /v1/owner/drain triggers asynchronous drain and returns 202.
    let drain_resp = client
        .post(format!("{base}/v1/owner/drain"))
        .header("X-AirCD-Invite", "drain-owner-key")
        .json(&json!({ "reason": "operator test" }))
        .send()
        .await?;
    assert_eq!(drain_resp.status(), StatusCode::ACCEPTED);
    let drain_json: Value = drain_resp.json().await?;
    assert_eq!(drain_json["ok"], true);
    assert_eq!(drain_json["draining"], true);

    // Simulate main() wait loop: wait for drain request, run graceful_drain, check metrics & clean HTTP exit.
    let drain_signal = server.shutdown().wait_for_drain_request().await;
    assert_eq!(drain_signal, "owner_api (operator test)");
    let report = server.graceful_drain(&drain_signal, None).await;
    assert_eq!(report.cancelled_pending_wakes, 1);
    assert_eq!(report.detached_wake_runs, 1);
    assert_eq!(report.checkpointed_tasks, 1);

    tokio::time::timeout(std::time::Duration::from_secs(2), http_task).await??;

    // Metrics reflect shutdown counters and draining gauge directly in rendered Prometheus text.
    let metrics_text = server.metrics().render_prometheus(&server.metrics_snapshot()?);
    assert_eq!(scrape_metric_u64(&metrics_text, "aircd_hub_draining"), 1);
    assert_eq!(
        scrape_metric_u64(&metrics_text, "aircd_shutdown_cancelled_wakes_total"),
        1
    );
    assert_eq!(
        scrape_metric_u64(&metrics_text, "aircd_shutdown_lease_checkpoints_total"),
        1
    );
    assert_eq!(
        scrape_metric_u64(&metrics_text, "aircd_shutdown_detached_workers_total"),
        1
    );
    assert_eq!(
        scrape_metric_u64(&metrics_text, "aircd_wake_runs_total{status=\"detached\"}"),
        1
    );

    Ok(())
}
