#[tokio::test]
async fn task_timeout_http_policy_validation_and_holder_authorization() -> Result<()> {
    let (api, tokens) = task_workflow_api().await?;
    for invalid in [json!(0), json!(-1), json!(1.5), json!(true), json!("30")] {
        let (status, _) = delegation_post(
            &api,
            &tokens["pm"],
            "/v1/tasks",
            json!({"channel":"#one", "title":"Invalid", "timeout_seconds":invalid}),
        )
        .await?;
        assert!((400..500).contains(&status));
    }
    let (status, body) = delegation_post(
        &api,
        &tokens["pm"],
        "/v1/tasks",
        json!({"channel":"#one", "title":"Timed", "assignee":"worker", "timeout_seconds":60}),
    )
    .await?;
    assert_eq!(status, 201, "{body}");
    let id = body["task"]["id"].as_str().unwrap();
    assert_eq!(body["task"]["timeout_seconds"], 60);
    let readonly = api.server.auth().issue_token_with_options(
        "worker",
        Some("read-only heartbeat"),
        false,
        Some(&["read".to_string()]),
        None,
    )?;
    let builder_pm = api.server.auth().issue_token_with_options(
        "pm",
        Some("write-only policy"),
        false,
        Some(&["write".to_string()]),
        None,
    )?;
    assert_eq!(
        delegation_post(
            &api,
            &readonly.token,
            &format!("/v1/tasks/{id}/heartbeat"),
            json!({})
        )
        .await?
        .0,
        403
    );
    assert_eq!(
        delegation_post(
            &api,
            &builder_pm.token,
            &format!("/v1/tasks/{id}/timeout"),
            json!({"timeout_seconds":30})
        )
        .await?
        .0,
        403
    );
    let policy = format!("/v1/tasks/{id}/timeout");
    let heartbeat = format!("/v1/tasks/{id}/heartbeat");
    for path in [&policy, &heartbeat] {
        assert_eq!(
            api.request("POST", path, None, None, Some(&json!({})))
                .await?
                .0,
            401
        );
        assert_eq!(
            api.request("POST", path, Some("invalid"), None, Some(&json!({})))
                .await?
                .0,
            401
        );
    }
    assert_eq!(
        delegation_post(
            &api,
            &tokens["worker"],
            &policy,
            json!({"timeout_seconds":90})
        )
        .await?
        .0,
        400
    );
    assert_eq!(
        delegation_post(&api, &tokens["worker"], &heartbeat, json!({}))
            .await?
            .0,
        400
    );
    assert_eq!(
        delegation_post(
            &api,
            &tokens["worker"],
            &format!("/v1/tasks/{id}/status"),
            json!({"status":"in_progress"})
        )
        .await?
        .0,
        200
    );
    for nick in ["pm", "peer"] {
        assert_eq!(
            delegation_post(&api, &tokens[nick], &heartbeat, json!({}))
                .await?
                .0,
            400
        );
    }
    let (status, body) = delegation_post(
        &api,
        &tokens["worker"],
        &heartbeat,
        json!({"actor":"peer", "execution_heartbeat_at":99999999999_i64}),
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    assert!(
        body["task"]["execution_heartbeat_at"].as_i64().unwrap()
            <= crate::history::unix_timestamp()
    );
    let (status, body) = delegation_post(
        &api,
        &tokens["pm"],
        &policy,
        json!({"timeout_seconds":null}),
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    assert!(body["task"]["timeout_seconds"].is_null());
    assert_eq!(
        delegation_post(&api, &tokens["pm"], &policy, json!({"timeout_seconds":0}))
            .await?
            .0,
        400
    );
    Ok(())
}

#[tokio::test]
async fn task_timeout_background_worker_recovers_and_notifies_exactly_once() -> Result<()> {
    let (api, _) = task_workflow_api().await?;
    let task = api.server.tasks().create_full(crate::tasks::NewTask {
        channel: "#one".into(),
        title: "Orphan".into(),
        assignee: Some("worker".into()),
        sprint: Some("#one-s1".into()),
        timeout_seconds: Some(30),
        ..Default::default()
    })?;
    api.server
        .tasks()
        .set_status(&task.id, "assigned", "testing", "worker", None, None)?;
    api.db.write()?.execute(
        "UPDATE tasks SET execution_heartbeat_at = 1 WHERE id = ?1",
        [&task.id],
    )?;
    let worker = api.server.spawn_task_timeout_worker();
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if api.server.get_task_api(&task.id)?.status == "assigned" {
                break Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;
    worker.abort();
    result??;
    assert_eq!(
        api.server
            .recover_task_timeouts(crate::history::unix_timestamp())?,
        0
    );
    let history = api.server.history().tail("#one-s1", 0, 100)?;
    let notices: Vec<_> = history
        .iter()
        .filter(|m| m.body.contains("timed out"))
        .collect();
    assert_eq!(notices.len(), 1);
    assert!(notices[0].body.contains("@worker") && notices[0].body.contains("@pm"));
    assert_eq!(
        api.server
            .tasks()
            .events(&task.id)?
            .iter()
            .filter(|e| e.action == "timeout")
            .count(),
        1
    );
    let metrics = api.server.render_prometheus_metrics()?;
    assert!(metrics.contains("action=\"timeout\""));
    Ok(())
}
