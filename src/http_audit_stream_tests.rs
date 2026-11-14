async fn read_sse_events_until(
    resp: &mut reqwest::Response,
    target_count: usize,
) -> Result<Vec<(String, String, Value)>> {
    let mut buf = String::new();
    let mut out = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while out.len() < target_count {
        let remain = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remain.is_zero() {
            anyhow::bail!(
                "timed out waiting for {target_count} SSE events (got {}: {:?}; raw={buf:?})",
                out.len(),
                out
            );
        }
        let chunk = tokio::time::timeout(remain, resp.chunk())
            .await
            .context("SSE chunk timeout")??;
        let Some(bytes) = chunk else {
            break;
        };
        buf.push_str(&String::from_utf8_lossy(&bytes));
        while let Some(pos) = buf.find("\n\n") {
            let block = buf[..pos].to_string();
            buf.drain(..pos + 2);
            let mut id = String::new();
            let mut event_name = String::new();
            let mut data_lines = Vec::new();
            for raw_line in block.lines() {
                let line = raw_line.trim_end_matches('\r');
                if line.is_empty() || line.starts_with(':') {
                    continue;
                }
                if let Some(rest) = line.strip_prefix("id:") {
                    id = rest.trim().to_string();
                } else if let Some(rest) = line.strip_prefix("event:") {
                    event_name = rest.trim().to_string();
                } else if let Some(rest) = line.strip_prefix("data:") {
                    data_lines.push(rest.trim_start().to_string());
                }
            }
            if !data_lines.is_empty() {
                let payload: Value = serde_json::from_str(&data_lines.join("\n"))?;
                out.push((id, event_name, payload));
                if out.len() >= target_count {
                    break;
                }
            }
        }
    }
    Ok(out)
}

async fn read_ndjson_events_until(
    resp: &mut reqwest::Response,
    target_count: usize,
) -> Result<Vec<Value>> {
    let mut buf = String::new();
    let mut out = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while out.len() < target_count {
        let remain = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remain.is_zero() {
            anyhow::bail!(
                "timed out waiting for {target_count} NDJSON events (got {}: {:?}; raw={buf:?})",
                out.len(),
                out
            );
        }
        let chunk = tokio::time::timeout(remain, resp.chunk())
            .await
            .context("NDJSON chunk timeout")??;
        let Some(bytes) = chunk else {
            break;
        };
        buf.push_str(&String::from_utf8_lossy(&bytes));
        while let Some(pos) = buf.find('\n') {
            let line = buf[..pos].trim().to_string();
            buf.drain(..pos + 1);
            if !line.is_empty() {
                out.push(serde_json::from_str(&line)?);
                if out.len() >= target_count {
                    break;
                }
            }
        }
    }
    Ok(out)
}

#[tokio::test]
async fn audit_stream_auth_enforcement_and_live_sse_ndjson_events() -> Result<()> {
    let db = store::open(":memory:")?;
    let server = Server::new(db);
    let (owner, admin_token) = server.auth().register_principal("owner", Some("owner"), None)?;
    let (pm, _) = server.auth().register_principal("pm", Some("leader"), None)?;
    let (worker, _) = server
        .auth()
        .register_principal("worker", Some("member"), None)?;
    server.projects().create("#cams", "CAMS", None, &pm.id)?;
    server.projects().set_pm("#cams", Some(&pm.id))?;
    server
        .projects()
        .set_role("#cams", &pm.id, "coordinator")?;
    server
        .projects()
        .set_role("#cams", &worker.id, "member")?;

    let read_token = server
        .auth()
        .issue_token_with_options(
            &worker.id,
            Some("siem-read-only"),
            false,
            Some(&["read".to_string()]),
            None,
        )?
        .token;

    let owner_secret = "audit-owner-secret";
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mut info = build_runtime_info("127.0.0.1:6667", &addr.to_string(), None);
    info.register_key = Some(owner_secret.to_string());
    let app = router(server.clone(), info);
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let client = reqwest::Client::new();
    let base = format!("http://{addr}");

    // 1. Auth checks: 401 without token, 403 for non-admin scope, 403 for wrong owner key.
    assert_eq!(
        client
            .get(format!("{base}/v1/audit/stream"))
            .send()
            .await?
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        client
            .get(format!("{base}/v1/audit/stream"))
            .bearer_auth(&read_token)
            .send()
            .await?
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        client
            .get(format!("{base}/v1/audit/stream"))
            .header("X-AirCD-Invite", "wrong-secret")
            .send()
            .await?
            .status(),
        StatusCode::FORBIDDEN
    );

    // 2. Bad filter query params return 400 Bad Request.
    assert_eq!(
        client
            .get(format!("{base}/v1/audit/stream?format=xml"))
            .bearer_auth(&admin_token)
            .send()
            .await?
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        client
            .get(format!("{base}/v1/audit/stream?category=unknown"))
            .bearer_auth(&admin_token)
            .send()
            .await?
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        client
            .get(format!("{base}/v1/audit/stream?limit=0"))
            .bearer_auth(&admin_token)
            .send()
            .await?
            .status(),
        StatusCode::BAD_REQUEST
    );

    // 3. Open live SSE stream with Scope::Admin bearer token (replaying earlier auth failures too).
    let mut sse_resp = client
        .get(format!("{base}/v1/audit/stream?replay=10&keepalive_secs=0"))
        .bearer_auth(&admin_token)
        .send()
        .await?;
    assert_eq!(sse_resp.status(), StatusCode::OK);
    assert_eq!(
        sse_resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream; charset=utf-8")
    );
    assert_eq!(
        sse_resp
            .headers()
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok()),
        Some("no-cache")
    );

    // Check Prometheus active subscriber gauge while stream is connected.
    let metrics_body = client
        .get(format!("{base}/metrics"))
        .send()
        .await?
        .text()
        .await?;
    assert_eq!(
        scrape_metric_u64(&metrics_body, "aircd_audit_stream_active_subscribers"),
        1
    );

    // Trigger live auth, security, and task_state events while SSE stream is open.
    let _ = client
        .post(format!("{base}/v1/tokens"))
        .bearer_auth(&admin_token)
        .json(&json!({
            "nick": "worker",
            "label": "audit-live-token",
            "scopes": ["write"]
        }))
        .send()
        .await?;

    server
        .set_role_api(&owner, &worker.id, "reviewer", Some("#cams"))?;

    let task = server.create_task_full_api(
        &pm,
        crate::tasks::NewTask {
            channel: "#cams".to_string(),
            title: "Audit stream test task".to_string(),
            kind: Some("feature".to_string()),
            assignee: Some(worker.id.clone()),
            created_by: Some(pm.id.clone()),
            ..Default::default()
        },
    )?;

    // Failed transition (missing test report on review) must NOT emit a task_state event.
    assert!(server
        .set_task_status_api(&worker, &task.id, "review", None, Some("feat/x"), None, None)
        .is_err());

    server.set_task_status_api(
        &worker,
        &task.id,
        "in_progress",
        None,
        Some("feat/x"),
        None,
        None,
    )?;

    // We expect at least: 3 prior auth rejections + token.issued + role.changed + task.created + task.status = 7 events.
    let sse_events = read_sse_events_until(&mut sse_resp, 7).await?;
    drop(sse_resp);

    let actions: Vec<&str> = sse_events
        .iter()
        .map(|(_, _, p)| p["action"].as_str().unwrap_or(""))
        .collect();
    assert!(actions.contains(&"auth.http_failed"), "got {actions:?}");
    assert!(actions.contains(&"auth.scope_denied"), "got {actions:?}");
    assert!(actions.contains(&"owner.auth_denied"), "got {actions:?}");
    assert!(actions.contains(&"token.issued"), "got {actions:?}");
    assert!(actions.contains(&"role.changed"), "got {actions:?}");
    assert!(actions.contains(&"task.created"), "got {actions:?}");
    assert!(actions.contains(&"task.status"), "got {actions:?}");
    // Exactly one task.status event (failed transition must not emit).
    assert_eq!(
        actions.iter().filter(|a| **a == "task.status").count(),
        1,
        "got {actions:?}"
    );

    let task_status_ev = sse_events
        .iter()
        .find(|(_, _, p)| p["action"] == "task.status")
        .unwrap();
    assert_eq!(task_status_ev.1, "task_state");
    assert_eq!(task_status_ev.2["category"], "task_state");
    assert_eq!(task_status_ev.2["task_id"], task.id);
    assert_eq!(task_status_ev.2["from_status"], "assigned");
    assert_eq!(task_status_ev.2["to_status"], "in_progress");
    assert_eq!(task_status_ev.2["channel"], "#cams");
    assert!(task_status_ev.2["timestamp_iso"]
        .as_str()
        .unwrap()
        .ends_with('Z'));

    // 4. Reconnect via Last-Event-ID and NDJSON chunked format with owner key + filters.
    let mid_seq = sse_events[2].2["seq"].as_u64().unwrap();
    let mut ndjson_resp = client
        .get(format!(
            "{base}/v1/audit/stream?format=ndjson&category=task_state&task_id={}&limit=2",
            task.id
        ))
        .header("X-AirCD-Invite", owner_secret)
        .header("Last-Event-ID", mid_seq.to_string())
        .send()
        .await?;
    assert_eq!(ndjson_resp.status(), StatusCode::OK);
    assert_eq!(
        ndjson_resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/x-ndjson; charset=utf-8")
    );
    let ndjson_events = read_ndjson_events_until(&mut ndjson_resp, 2).await?;
    assert_eq!(ndjson_events.len(), 2);
    assert_eq!(ndjson_events[0]["action"], "task.created");
    assert_eq!(ndjson_events[1]["action"], "task.status");
    assert!(ndjson_events[0]["seq"].as_u64().unwrap() > mid_seq);
    // Stream closes cleanly at EOF after limit=2.
    assert!(ndjson_resp.chunk().await?.is_none());

    // 5. Accept: application/x-ndjson negotiates chunked NDJSON without ?format=.
    let mut accept_ndjson = client
        .get(format!("{base}/v1/audit/stream?replay=1&limit=1"))
        .header(header::ACCEPT, "application/x-ndjson")
        .bearer_auth(&admin_token)
        .send()
        .await?;
    assert_eq!(
        accept_ndjson
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/x-ndjson; charset=utf-8")
    );
    let single = read_ndjson_events_until(&mut accept_ndjson, 1).await?;
    assert_eq!(single.len(), 1);
    assert!(accept_ndjson.chunk().await?.is_none());

    handle.abort();
    drop(server);
    Ok(())
}

#[tokio::test]
async fn audit_stream_closes_cleanly_on_hub_shutdown() -> Result<()> {
    let db = store::open(":memory:")?;
    let server = Server::new(db);
    let (_, admin_token) = server.auth().register_principal("owner", Some("owner"), None)?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let info = build_runtime_info("127.0.0.1:6667", &addr.to_string(), None);
    let stop_rx = server.shutdown().stop_accepting_rx();

    let serve_server = server.clone();
    let serve_handle = tokio::spawn(async move {
        serve_on_with_shutdown(listener, serve_server, info, stop_rx).await
    });

    let client = reqwest::Client::new();
    let mut resp = client
        .get(format!("http://{addr}/v1/audit/stream?keepalive_secs=0"))
        .bearer_auth(&admin_token)
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::OK);

    // Read the initial `: stream-open` chunk so subscription is active.
    let first_chunk = tokio::time::timeout(std::time::Duration::from_secs(2), resp.chunk())
        .await??
        .expect("initial SSE banner");
    assert!(String::from_utf8_lossy(&first_chunk).contains(": stream-open"));

    // Trigger hub shutdown stop_accepting: stream must reach EOF and axum::serve must finish cleanly.
    server.shutdown().trigger_stop_accepting();
    let eof = tokio::time::timeout(std::time::Duration::from_secs(2), resp.chunk()).await??;
    assert!(eof.is_none(), "expected clean EOF on shutdown, got {eof:?}");

    tokio::time::timeout(std::time::Duration::from_secs(2), serve_handle).await???;
    Ok(())
}
