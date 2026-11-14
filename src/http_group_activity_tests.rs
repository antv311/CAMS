#[tokio::test]
async fn inactivity_http_heartbeat_and_events_require_current_named_membership() -> Result<()> {
    let (api, tokens) = task_workflow_api().await?;
    attach_delegation_group(&api).await?;
    for endpoint in ["heartbeat", "events"] {
        let method = if endpoint == "heartbeat" {
            "POST"
        } else {
            "GET"
        };
        let path = format!("/v1/groups/team/{endpoint}");
        for token in [None, Some("invalid")] {
            assert_eq!(
                api.request(method, &path, token, None, None).await?.0,
                StatusCode::UNAUTHORIZED
            );
        }
        // An owner credential cannot substitute for roster membership.
        assert_eq!(
            api.request(
                method,
                &path,
                Some("human-token"),
                Some("template-owner-key"),
                None
            )
            .await?
            .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            api.request(
                method,
                &format!("/v1/groups/missing/{endpoint}"),
                Some(&tokens["worker"]),
                None,
                None
            )
            .await?
            .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            api.request(method, &path, Some(&tokens["worker"]), None, None)
                .await?
                .0,
            StatusCode::OK
        );
    }
    let worker = api.server.auth().resolve_by_name("worker")?.unwrap();
    let before: i64 = api.db.read()?.query_row(
        "SELECT last_activity_at FROM principal_activity WHERE principal_id = ?1",
        [&worker.id],
        |row| row.get(0),
    )?;
    let (status, body) = api
        .request(
            "POST",
            "/v1/groups/team/heartbeat",
            Some(&tokens["worker"]),
            None,
            Some(&json!({"principal_id": "peer", "last_activity_at": 9999999999_i64})),
        )
        .await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["principal_id"], worker.id);
    assert!(body["last_activity_at"].as_i64().unwrap() >= before);
    assert!(body["last_activity_at"].as_i64().unwrap() <= crate::history::unix_timestamp());
    api.write(
        "PATCH",
        "/v1/groups/team",
        Some(&json!({
            "remove_workers": [{"template":"example", "model":"worker-model", "nick":"Worker"}]
        })),
    )
    .await?;
    for (method, path) in [
        ("POST", "/v1/groups/team/heartbeat"),
        ("GET", "/v1/groups/team/events"),
    ] {
        assert_eq!(
            api.request(method, path, Some(&tokens["worker"]), None, None)
                .await?
                .0,
            StatusCode::FORBIDDEN
        );
    }
    Ok(())
}

#[tokio::test]
async fn inactivity_http_policy_and_automatic_notice_wake_reach_new_assignee() -> Result<()> {
    let (api, tokens) = task_workflow_api().await?;
    let mut group = delegation_group();
    for invalid in [json!(0), json!(-1), json!(1.5), json!("60"), json!(true)] {
        group["inactivity_timeout_secs"] = invalid;
        let response = api
            .client
            .post(format!("{}/v1/groups", api.base))
            .header("X-AirCD-Invite", "template-owner-key")
            .json(&group)
            .send()
            .await?;
        assert!(response.status().is_client_error());
        assert!(api.server.groups().get("team")?.is_none());
    }
    group["inactivity_timeout_secs"] = json!(60);
    let (status, body) = api.write("POST", "/v1/groups", Some(&group)).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    api.write(
        "POST",
        "/v1/groups/team/attach",
        Some(&json!({"channel":"#one"})),
    )
    .await?;
    let owner = api.server.auth().resolve_by_name("human")?.unwrap();
    api.server.open_room_api(&owner, "#one", None)?;
    api.server.wakes().upsert(
        "peer",
        crate::wake::WakeConfigInput {
            command: std::env::current_exe()?.to_string_lossy().into_owned(),
            args: vec![
                "--exact".into(),
                "http::tests::delegation_probe_child".into(),
                "--nocapture".into(),
            ],
            env: std::collections::BTreeMap::from([
                ("AIRCD_DELEGATION_PROBE".into(), Some("1".into())),
                ("DELEGATION_PROMPT".into(), Some("{prompt}".into())),
            ]),
            cooldown_seconds: Some(0),
            ..Default::default()
        },
    )?;
    let task = api.server.tasks().create_full(crate::tasks::NewTask {
        channel: "#one".into(),
        title: "Resume stalled task".into(),
        assignee: Some("worker".into()),
        sprint: Some("#one-s1".into()),
        ..Default::default()
    })?;
    let now = crate::history::unix_timestamp();
    {
        let db = api.db.write()?;
        db.execute("UPDATE groups SET updated_at = ?1", [now - 61])?;
        db.execute("UPDATE tasks SET updated_at = ?1", [now - 61])?;
    }
    // Exercise the actual timer instead of invoking the sweep by hand.
    let worker = api.server.spawn_group_inactivity_worker();
    let result = expect_delegation_wake(&api, 1, "peer", &task.id).await;
    worker.abort();
    result?;
    let current = api.server.get_task_api(&task.id)?;
    assert_eq!(current.assignee.as_deref(), Some("peer"));
    assert_eq!(current.status, "assigned");
    let notices = api.server.history().tail("#one-s1", 0, 100)?;
    let notice = notices
        .iter()
        .find(|m| m.body.contains("automatically reassigned"))
        .unwrap();
    assert!(notice.body.contains("from @worker to @peer"));
    assert!(notice.body.contains("group team"));
    let (status, events) = api
        .request(
            "GET",
            "/v1/groups/team/events",
            Some(&tokens["lead"]),
            None,
            None,
        )
        .await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(events["events"].as_array().unwrap().len(), 1);
    assert_eq!(events["events"][0]["task_id"], task.id);
    assert_eq!(
        api.server.tasks().events(&task.id)?[0].action,
        "auto_reassign"
    );
    Ok(())
}
