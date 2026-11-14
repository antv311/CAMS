#[tokio::test]
async fn prometheus_alert_rules_exporter_and_auth_redaction() -> Result<()> {
    let dir = std::env::temp_dir().join(format!("aircd-alerts-rules-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir)?;
    let db = store::open(dir.join("hub.sqlite3"))?;
    let server = Server::new(db);
    let mut cfg = crate::alerts::AlertConfig::default();
    cfg.group_name = "cams_test_alerts".to_string();
    cfg.eval_interval_secs = 20;
    cfg.webhook_url = Some("http://127.0.0.1:18999/secret-hook-path".to_string());
    cfg.webhook_secret = Some("top-secret-hmac-key".to_string());
    cfg.disk_pressure_enabled = false;
    server.alerts().update_config(cfg);

    let owner_secret = "owner-alerts-secret";
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

    // 1. Public YAML rules endpoint works without authentication and omits secrets.
    let yaml_resp = client
        .get(format!("{base}/metrics/alerts/rules"))
        .send()
        .await?;
    assert_eq!(yaml_resp.status(), StatusCode::OK);
    assert_eq!(
        yaml_resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/yaml; charset=utf-8")
    );
    let yaml = yaml_resp.text().await?;
    assert!(yaml.contains("name: \"cams_test_alerts\""));
    assert!(yaml.contains("interval: \"20s\""));
    assert!(yaml.contains("alert: \"AircdHighWakeFailureRate\""));
    assert!(yaml.contains("alert: \"AircdQueueBacklogHigh\""));
    assert!(!yaml.contains("AircdDiskPressureHigh"));
    assert!(!yaml.contains("top-secret-hmac-key"));
    assert!(!yaml.contains("secret-hook-path"));

    // include_disabled=true exports disabled rules as well; format=json returns structured groups.
    let json_resp: Value = client
        .get(format!("{base}/metrics/alerts/rules?format=json&include_disabled=true"))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(json_resp["ok"], true);
    let rules = json_resp["groups"][0]["rules"].as_array().unwrap();
    assert_eq!(rules.len(), 3);

    // 2. /v1/alerts requires Read token and redacts webhook_url.
    assert_eq!(
        client
            .get(format!("{base}/v1/alerts"))
            .send()
            .await?
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let (_, member_token) = server
        .auth()
        .register_principal("alert-reader", Some("member"), None)?;
    let alerts_resp = client
        .get(format!("{base}/v1/alerts"))
        .bearer_auth(&member_token)
        .send()
        .await?;
    assert_eq!(alerts_resp.status(), StatusCode::OK);
    let alerts_json: Value = alerts_resp.json().await?;
    assert_eq!(alerts_json["alerts"]["webhook_configured"], true);
    assert!(alerts_json["alerts"]["webhook_url"].is_null());
    assert_eq!(alerts_json["alerts"]["webhook_secret_configured"], true);
    assert!(!alerts_json.to_string().contains("top-secret-hmac-key"));

    // 3. Owner endpoints reject bearer tokens and expose non-secret config to owner.
    assert_eq!(
        client
            .get(format!("{base}/v1/owner/alerts"))
            .bearer_auth(&member_token)
            .send()
            .await?
            .status(),
        StatusCode::FORBIDDEN
    );
    let owner_json: Value = client
        .get(format!("{base}/v1/owner/alerts"))
        .header("X-AirCD-Invite", owner_secret)
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(owner_json["ok"], true);
    assert_eq!(
        owner_json["alerts"]["webhook_url"],
        "http://127.0.0.1:18999/secret-hook-path"
    );
    assert!(owner_json["config"].get("webhook_secret").is_none());

    handle.abort();
    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

#[tokio::test]
async fn alert_evaluator_delivers_signed_webhooks_for_wake_backlog_and_disk() -> Result<()> {
    let dir = std::env::temp_dir().join(format!("aircd-alerts-e2e-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir)?;
    let db = store::open(dir.join("hub.sqlite3"))?;
    let server = Server::new(db);
    let _wh_worker = server.webhooks().spawn_worker();

    // Start local webhook receivers: one for direct server alerts, one for principal alerts.
    let (direct_tx, mut direct_rx) = mpsc::unbounded_channel::<(String, String, String)>();
    let (principal_tx, mut principal_rx) = mpsc::unbounded_channel::<(String, String, String)>();

    let hook_router = Router::new()
        .route(
            "/server-alerts",
            post({
                let direct_tx = direct_tx.clone();
                move |headers: HeaderMap, body: Bytes| {
                    let direct_tx = direct_tx.clone();
                    async move {
                        let event = headers
                            .get("x-aircd-event")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or_default()
                            .to_string();
                        let sig = headers
                            .get("x-aircd-signature-256")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or_default()
                            .to_string();
                        let raw = String::from_utf8_lossy(&body).to_string();
                        let _ = direct_tx.send((event, sig, raw));
                        StatusCode::OK
                    }
                }
            }),
        )
        .route(
            "/principal-alerts",
            post({
                let principal_tx = principal_tx.clone();
                move |headers: HeaderMap, body: Bytes| {
                    let principal_tx = principal_tx.clone();
                    async move {
                        let event = headers
                            .get("x-aircd-event")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or_default()
                            .to_string();
                        let sig = headers
                            .get("x-aircd-signature-256")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or_default()
                            .to_string();
                        let raw = String::from_utf8_lossy(&body).to_string();
                        let _ = principal_tx.send((event, sig, raw));
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

    let owner_secret = "owner-alert-e2e";
    let direct_secret = "direct-alert-hmac-secret";
    let mut alert_cfg = crate::alerts::AlertConfig::default();
    alert_cfg.webhook_url = Some(format!("http://{hook_addr}/server-alerts"));
    alert_cfg.webhook_secret = Some(direct_secret.to_string());
    alert_cfg.wake_failure_rate_threshold = 0.5;
    alert_cfg.wake_failure_min_samples = 3;
    alert_cfg.wake_failure_for_secs = 0;
    alert_cfg.queue_backlog_threshold = 2;
    alert_cfg.queue_backlog_for_secs = 0;
    // Ensure disk pressure doesn't trip until we deliberately raise min_free_bytes below.
    alert_cfg.disk_used_ratio_threshold = 0.0;
    alert_cfg.disk_min_free_bytes = 0;
    alert_cfg.disk_pressure_for_secs = 0;
    server.alerts().update_config(alert_cfg.clone());

    let api_listener = TcpListener::bind("127.0.0.1:0").await?;
    let api_addr = api_listener.local_addr()?;
    let mut info = build_runtime_info("127.0.0.1:6667", &api_addr.to_string(), None);
    info.register_key = Some(owner_secret.to_string());
    let app = router(server.clone(), info);
    let api_handle = tokio::spawn(async move {
        let _ = axum::serve(api_listener, app).await;
    });

    let client = reqwest::Client::new();
    let base = format!("http://{api_addr}");

    // Register a principal subscribed to "alert" webhook events.
    let (_, ops_token) = server
        .auth()
        .register_principal("ops-bot", Some("member"), None)?;
    let wh_resp: Value = client
        .post(format!("{base}/v1/webhooks"))
        .bearer_auth(&ops_token)
        .json(&json!({
            "url": format!("http://{hook_addr}/principal-alerts"),
            "events": ["alert"],
            "notify_when": "mentions"
        }))
        .send()
        .await?
        .json()
        .await?;
    let principal_secret = wh_resp["secret"].as_str().unwrap().to_string();

    // 1. Test alert webhook connectivity via /v1/owner/alerts/test.
    let test_resp = client
        .post(format!("{base}/v1/owner/alerts/test"))
        .header("X-AirCD-Invite", owner_secret)
        .json(&json!({}))
        .send()
        .await?;
    assert_eq!(test_resp.status(), StatusCode::OK);

    let (ev, sig, raw) = tokio::time::timeout(std::time::Duration::from_secs(5), direct_rx.recv())
        .await?
        .expect("direct test alert");
    assert_eq!(ev, "alert");
    assert_eq!(sig, crate::webhooks::sign_payload(direct_secret, raw.as_bytes()));
    let parsed: Value = serde_json::from_str(&raw)?;
    assert_eq!(parsed["status"], "test");
    assert_eq!(parsed["alertname"], "AircdAlertWebhookTest");

    let (p_ev, p_sig, p_raw) =
        tokio::time::timeout(std::time::Duration::from_secs(5), principal_rx.recv())
            .await?
            .expect("principal test alert");
    assert_eq!(p_ev, "alert");
    assert_eq!(
        p_sig,
        crate::webhooks::sign_payload(&principal_secret, p_raw.as_bytes())
    );

    // 2. Trigger sustained high wake failure rate (2 failed, 1 ok -> 66.7% >= 50%).
    server.metrics().record_wake_completion("failed", 0.2, 0.3);
    server.metrics().record_wake_completion("timeout", 1.0, 1.1);
    server.metrics().record_wake_completion("ok", 0.1, 0.2);

    // Also enqueue 2 pending webhook deliveries for an unreachable endpoint to trip queue backlog >= 2.
    server.webhooks().upsert(
        &ops_token_principal_id(&server, "ops-bot"),
        "http://127.0.0.1:1/unreachable",
        Some(vec!["message".to_string()]),
        Some("always"),
        false,
    )?;
    // Use direct DB insert for 2 pending deliveries so queue_backlog_total == 2 deterministically.
    {
        let db_conn = server.db().write()?;
        for i in 0..2 {
            db_conn.execute(
                "INSERT INTO webhook_deliveries
                 (id, principal_id, event_type, channel, payload_json, status, attempts, next_attempt_at, created_at, updated_at)
                 VALUES (?1, (SELECT id FROM principals WHERE nick = 'ops-bot'), 'message', '#general', '{}', 'pending', 0, 9999999999, 1, 1)",
                rusqlite::params![format!("pending-backlog-{i}")],
            )?;
        }
    }

    // Also set disk_min_free_bytes to u64::MAX so disk pressure trips on the file DB volume.
    alert_cfg.disk_min_free_bytes = u64::MAX;
    server.alerts().update_config(alert_cfg.clone());

    let eval_resp: Value = client
        .post(format!("{base}/v1/owner/alerts/evaluate"))
        .header("X-AirCD-Invite", owner_secret)
        .json(&json!({}))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(eval_resp["ok"], true);
    assert_eq!(eval_resp["alerts"]["active_count"], 3);

    // All three alerts should arrive on the direct alert webhook.
    let mut fired_names = Vec::new();
    for _ in 0..3 {
        let (_, _, body) = tokio::time::timeout(std::time::Duration::from_secs(5), direct_rx.recv())
            .await?
            .expect("firing alert delivery");
        let v: Value = serde_json::from_str(&body)?;
        assert_eq!(v["status"], "firing");
        fired_names.push(v["alertname"].as_str().unwrap().to_string());
    }
    assert!(fired_names.contains(&"AircdHighWakeFailureRate".to_string()));
    assert!(fired_names.contains(&"AircdQueueBacklogHigh".to_string()));
    assert!(fired_names.contains(&"AircdDiskPressureHigh".to_string()));

    // Scrape /metrics and confirm alert gauges/counters + disk gauges are emitted validly.
    let metrics_body = client.get(format!("{base}/metrics")).send().await?.text().await?;
    assert!(metrics_body.contains("aircd_alerts_active{rule=\"wake_failure_rate\",alertname=\"AircdHighWakeFailureRate\",severity=\"critical\"} 1\n"));
    assert!(metrics_body.contains("aircd_alerts_active{rule=\"queue_backlog\",alertname=\"AircdQueueBacklogHigh\",severity=\"warning\"} 1\n"));
    assert!(metrics_body.contains("aircd_alerts_active{rule=\"disk_pressure\",alertname=\"AircdDiskPressureHigh\",severity=\"critical\"} 1\n"));
    assert!(metrics_body.contains("aircd_queue_backlog_total 2\n"));
    assert!(metrics_body.contains("aircd_disk_total_bytes "));
    assert!(metrics_body.contains("aircd_disk_available_bytes "));

    // Clear disk pressure and queue backlog, then re-evaluate -> resolved notifications delivered.
    {
        let db_conn = server.db().write()?;
        db_conn.execute("DELETE FROM webhook_deliveries WHERE id LIKE 'pending-backlog-%'", [])?;
    }
    alert_cfg.disk_min_free_bytes = 0;
    server.alerts().update_config(alert_cfg);

    let resolve_resp: Value = client
        .post(format!("{base}/v1/owner/alerts/evaluate"))
        .header("X-AirCD-Invite", owner_secret)
        .json(&json!({}))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(resolve_resp["alerts"]["active_count"], 1);

    let mut resolved_names = Vec::new();
    for _ in 0..2 {
        let (_, _, body) = tokio::time::timeout(std::time::Duration::from_secs(5), direct_rx.recv())
            .await?
            .expect("resolved alert delivery");
        let v: Value = serde_json::from_str(&body)?;
        assert_eq!(v["status"], "resolved");
        resolved_names.push(v["alertname"].as_str().unwrap().to_string());
    }
    assert!(resolved_names.contains(&"AircdQueueBacklogHigh".to_string()));
    assert!(resolved_names.contains(&"AircdDiskPressureHigh".to_string()));

    api_handle.abort();
    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

fn ops_token_principal_id(server: &Server, nick: &str) -> String {
    let db = server.db().read().unwrap();
    db.query_row(
        "SELECT id FROM principals WHERE nick = ?1",
        rusqlite::params![nick],
        |row| row.get(0),
    )
    .unwrap()
}
