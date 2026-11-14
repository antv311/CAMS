#[tokio::test]
async fn dashboard_serves_static_web_ui_assets() -> Result<()> {
    let api = TemplateApi::start(None).await?;

    let welcome = api.client.get(format!("{}/", api.base)).send().await?;
    assert_eq!(welcome.status(), StatusCode::OK);
    let welcome_html = welcome.text().await?;
    assert!(welcome_html.contains("href=\"/dashboard\""));

    for path in ["/dashboard", "/dashboard/"] {
        let resp = api.client.get(format!("{}{path}", api.base)).send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/html; charset=utf-8")
        );
        assert_eq!(
            resp.headers()
                .get(header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some("no-cache")
        );
        let html = resp.text().await?;
        assert!(html.contains("Sprint Monitor"));
        assert!(html.contains("/dashboard/style.css"));
        assert!(html.contains("/dashboard/app.js"));
        assert!(!html.contains(&api.token));
    }

    let css_resp = api
        .client
        .get(format!("{}/dashboard/style.css", api.base))
        .send()
        .await?;
    assert_eq!(css_resp.status(), StatusCode::OK);
    assert_eq!(
        css_resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/css; charset=utf-8")
    );
    let css = css_resp.text().await?;
    assert!(css.contains(".board-col"));

    let js_resp = api
        .client
        .get(format!("{}/dashboard/app.js", api.base))
        .send()
        .await?;
    assert_eq!(js_resp.status(), StatusCode::OK);
    assert_eq!(
        js_resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/javascript; charset=utf-8")
    );
    let js = js_resp.text().await?;
    assert!(js.contains("/v1/dashboard"));

    Ok(())
}

#[tokio::test]
async fn dashboard_api_enforces_auth_scopes() -> Result<()> {
    let api = TemplateApi::start(None).await?;

    // Unauthenticated request returns 401.
    let unauth = api
        .client
        .get(format!("{}/v1/dashboard", api.base))
        .send()
        .await?;
    assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);

    // Invalid bearer token returns 401.
    let invalid = api
        .client
        .get(format!("{}/v1/dashboard", api.base))
        .bearer_auth("not-a-valid-token")
        .send()
        .await?;
    assert_eq!(invalid.status(), StatusCode::UNAUTHORIZED);

    // Read-scoped token succeeds.
    let (read_principal, _) = api
        .server
        .auth()
        .register_principal("ro-viewer", Some("member"), None)?;
    let read_only_token = api
        .server
        .auth()
        .issue_token_with_scopes(
            &read_principal.id,
            None,
            true,
            &[crate::auth::Scope::Read],
            None,
        )?
        .token;
    let ok = api
        .client
        .get(format!("{}/v1/dashboard", api.base))
        .bearer_auth(&read_only_token)
        .send()
        .await?;
    assert_eq!(ok.status(), StatusCode::OK);
    let body: Value = ok.json().await?;
    assert_eq!(body["ok"], true);
    assert_eq!(body["viewer"]["nick"], "ro-viewer");

    Ok(())
}

#[tokio::test]
async fn dashboard_api_tracks_sprints_tasks_questions_and_agents_without_leaking_secrets(
) -> Result<()> {
    let api = TemplateApi::start(None).await?;

    let (pm, _) = api
        .server
        .auth()
        .register_principal("pm-lead", Some("leader"), None)?;
    let (builder, _) = api
        .server
        .auth()
        .register_principal("dev-1", Some("builder"), None)?;

    api.server
        .create_project_api(&pm, "#webdash", "Web Dashboard", Some("Sprint monitoring"))?;
    api.server
        .set_project_member_api(&pm, "#webdash", "dev-1", Some("builder"))?;
    api.server.projects().set_pm("#webdash", Some(&pm.id))?;

    let s1 = api
        .server
        .open_room_api(&pm, "#webdash", Some("Sprint 1"))?;
    let s2 = api
        .server
        .open_room_api(&pm, "#webdash", Some("Sprint 2"))?;

    // Configure a launcher with sensitive env/prompt/command to ensure /v1/dashboard never leaks them.
    let mut env = std::collections::BTreeMap::new();
    env.insert(
        "SECRET_API_KEY".to_string(),
        Some("super-secret-value-123".to_string()),
    );
    api.server.wakes().upsert(
        &builder.id,
        crate::wake::WakeConfigInput {
            command: "secret-agent-cli".to_string(),
            args: vec!["--secret-flag".to_string()],
            env,
            prompt: Some("top-secret-system-prompt".to_string()),
            enabled: Some(false),
            ..Default::default()
        },
    )?;

    // Task 1 in Sprint 1: complete full lifecycle to merged.
    let t1 = api.server.create_task_full_api(
        &pm,
        crate::tasks::NewTask {
            channel: s1.channel.clone(),
            title: "Build dashboard backend".to_string(),
            kind: Some("feature".to_string()),
            assignee: Some("dev-1".to_string()),
            branch: Some("feat/dash-api".to_string()),
            ..Default::default()
        },
    )?;
    api.server.set_task_status_api(
        &builder,
        &t1.id,
        "in_progress",
        Some("started"),
        Some("feat/dash-api"),
        None,
        None,
    )?;
    api.server.set_task_status_api(
        &builder,
        &t1.id,
        "testing",
        Some("testing"),
        Some("feat/dash-api"),
        None,
        None,
    )?;
    api.server.set_task_status_api(
        &builder,
        &t1.id,
        "review",
        Some("all unit tests pass"),
        Some("feat/dash-api"),
        None,
        None,
    )?;
    api.server.set_task_status_api(
        &pm,
        &t1.id,
        "merged",
        Some("merged to main"),
        Some("feat/dash-api"),
        None,
        None,
    )?;

    // Task 2 in Sprint 2: blocked with a pending question.
    let t2 = api.server.create_task_full_api(
        &pm,
        crate::tasks::NewTask {
            channel: "#webdash".to_string(),
            title: "Add sprint filter".to_string(),
            kind: Some("bug".to_string()),
            assignee: Some("dev-1".to_string()),
            sprint: Some(s2.channel.clone()),
            ..Default::default()
        },
    )?;
    api.server.set_task_status_api(
        &builder,
        &t2.id,
        "in_progress",
        None,
        None,
        None,
        None,
    )?;
    api.server
        .ask_task_api(&builder, &t2.id, "Should completed tasks show by default?")?;

    // Task 3 in Sprint 2: in_progress.
    let t3 = api.server.create_task_full_api(
        &pm,
        crate::tasks::NewTask {
            channel: "#webdash".to_string(),
            title: "Polish dark CSS".to_string(),
            kind: Some("task".to_string()),
            assignee: Some("dev-1".to_string()),
            sprint: Some(s2.channel.clone()),
            ..Default::default()
        },
    )?;
    api.server.set_task_status_api(
        &builder,
        &t3.id,
        "in_progress",
        None,
        None,
        None,
        None,
    )?;

    // Unfiltered dashboard snapshot includes all 3 tasks (including merged Task 1).
    let resp = api
        .client
        .get(format!("{}/v1/dashboard", api.base))
        .bearer_auth(&api.token)
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let raw_text = resp.text().await?;
    assert!(!raw_text.contains("super-secret-value-123"));
    assert!(!raw_text.contains("top-secret-system-prompt"));
    assert!(!raw_text.contains("secret-agent-cli"));
    assert!(!raw_text.contains("--secret-flag"));

    let body: Value = serde_json::from_str(&raw_text)?;
    assert_eq!(body["summary"]["projects_total"], 1);
    assert_eq!(body["summary"]["rooms_total"], 2);
    assert_eq!(body["summary"]["filtered_task_counts"]["total"], 3);
    assert_eq!(body["summary"]["filtered_task_counts"]["completed"], 1);
    assert_eq!(body["summary"]["filtered_task_counts"]["blocked"], 1);
    assert_eq!(body["summary"]["filtered_task_counts"]["in_progress"], 1);
    assert_eq!(body["summary"]["pending_questions_total"], 1);
    assert_eq!(body["tasks"].as_array().unwrap().len(), 3);
    assert_eq!(body["pending_questions"][0]["task_id"], t2.id);

    let rooms = body["rooms"].as_array().unwrap();
    let r1 = rooms.iter().find(|r| r["channel"] == "#webdash-s1").unwrap();
    let r2 = rooms.iter().find(|r| r["channel"] == "#webdash-s2").unwrap();
    assert_eq!(r1["task_counts"]["completed"], 1);
    assert_eq!(r1["task_counts"]["total"], 1);
    assert_eq!(r2["task_counts"]["blocked"], 1);
    assert_eq!(r2["task_counts"]["in_progress"], 1);
    assert_eq!(r2["task_counts"]["total"], 2);

    let dev_agent = body["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["nick"] == "dev-1")
        .unwrap();
    assert_eq!(dev_agent["active_task_count"], 2);
    assert_eq!(dev_agent["wake"]["launcher_configured"], true);
    assert_eq!(dev_agent["wake"]["launcher_enabled"], false);
    assert_eq!(dev_agent["wake"]["running"], false);

    // Filter by sprint room #webdash-s2.
    let s2_body: Value = api
        .client
        .get(format!("{}/v1/dashboard?channel=webdash&sprint=webdash-s2", api.base))
        .bearer_auth(&api.token)
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(s2_body["tasks"].as_array().unwrap().len(), 2);
    assert_eq!(s2_body["summary"]["filtered_task_counts"]["total"], 2);
    assert_eq!(s2_body["summary"]["filtered_task_counts"]["completed"], 0);

    // Filter with include_terminal=false omits merged Task 1 from tasks array,
    // while keeping accurate completion count in filtered_task_counts.
    let active_body: Value = api
        .client
        .get(format!("{}/v1/dashboard?include_terminal=false", api.base))
        .bearer_auth(&api.token)
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(active_body["tasks"].as_array().unwrap().len(), 2);
    assert_eq!(active_body["summary"]["filtered_task_counts"]["total"], 3);
    assert_eq!(active_body["summary"]["filtered_task_counts"]["completed"], 1);

    Ok(())
}
