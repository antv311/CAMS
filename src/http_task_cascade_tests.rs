#[tokio::test]
async fn task_cascade_http_enforces_authority_cleans_inboxes_and_preserves_child_history(
) -> Result<()> {
    let (api, tokens) = task_workflow_api().await?;
    let (_, parent) = delegation_post(
        &api,
        &tokens["pm"],
        "/v1/tasks",
        json!({"channel": "#one", "title": "Parent", "assignee": "lead"}),
    )
    .await?;
    let parent = parent["task"]["id"].as_str().unwrap();
    let (_, child) = delegation_post(&api, &tokens["lead"], "/v1/tasks",
        json!({"channel": "#one", "title": "Child", "assignee": "worker", "parent_task_id": parent})).await?;
    let child = child["task"]["id"].as_str().unwrap();
    let (status, _) = delegation_post(
        &api,
        &tokens["worker"],
        &format!("/v1/tasks/{child}/ask"),
        json!({"question": "Child decision"}),
    )
    .await?;
    assert_eq!(status, 200);
    let before = api.server.history().tail("#one", 0, 100)?.len();
    let (status, _) = delegation_post(
        &api,
        &tokens["lead"],
        &format!("/v1/tasks/{parent}/status"),
        json!({"status": "cancelled"}),
    )
    .await?;
    assert_eq!(status, 400, "Only project management can cancel a tree");
    assert_eq!(api.server.history().tail("#one", 0, 100)?.len(), before);
    let (status, _) = delegation_post(
        &api,
        &tokens["pm"],
        &format!("/v1/tasks/{parent}/status"),
        json!({"status": "cancelled", "note": "No longer needed"}),
    )
    .await?;
    assert_eq!(status, 200);
    let worker = api.server.auth().resolve_by_name("worker")?.unwrap();
    let lead = api.server.auth().resolve_by_name("lead")?.unwrap();
    let pm = api.server.auth().resolve_by_name("pm")?.unwrap();
    assert_eq!(api.server.inbox_api(&worker)?["task_count"], 0);
    assert_eq!(api.server.inbox_api(&lead)?["question_count"], 0);
    assert_eq!(api.server.inbox_api(&pm)?["question_count"], 0);
    let (status, body) = call(
        &api.client,
        "GET",
        format!("{}/v1/tasks/{parent}", api.base),
        &tokens["lead"],
        Value::Null,
    )
    .await?;
    assert_eq!(status, 200);
    assert_eq!(body["subtasks"][0]["status"], "cancelled");
    let (status, body) = call(
        &api.client,
        "GET",
        format!("{}/v1/tasks/{child}", api.base),
        &tokens["worker"],
        Value::Null,
    )
    .await?;
    assert_eq!(status, 200);
    assert_eq!(body["task"]["parent_task_id"], parent);
    assert!(body["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["action"] == "parent_cancel"));
    let history = api.server.history().tail("#one", 0, 100)?;
    let notice = history
        .iter()
        .find(|m| m.body.contains(child) && m.body.contains("because ancestor"))
        .unwrap();
    assert!(notice.body.contains("cc @worker"));
    let before = history.len();
    for (token, path, body) in [
        (
            &tokens["lead"],
            "/v1/tasks".to_string(),
            json!({"channel": "#one", "title": "Late", "parent_task_id": parent}),
        ),
        (
            &tokens["pm"],
            format!("/v1/tasks/{child}/answer"),
            json!({"answer": "Stale"}),
        ),
        (
            &tokens["pm"],
            format!("/v1/tasks/{parent}/status"),
            json!({"status": "cancelled"}),
        ),
    ] {
        assert_eq!(delegation_post(&api, token, &path, body).await?.0, 400);
    }
    assert_eq!(api.server.history().tail("#one", 0, 100)?.len(), before);
    Ok(())
}

#[tokio::test]
async fn task_cascade_routes_real_stop_resume_wakes_to_assignees_and_former_lease_holders(
) -> Result<()> {
    let (api, tokens) = task_workflow_api().await?;
    let owner = api.server.auth().resolve_by_name("human")?.unwrap();
    api.server.open_room_api(&owner, "#one", None)?;
    api.server.gits().upsert(
        "repo-one",
        "#one",
        &std::env::current_dir()?.to_string_lossy(),
        None,
        None,
        None,
    )?;
    let (_, parent) = delegation_post(
        &api,
        &tokens["pm"],
        "/v1/tasks",
        json!({"channel": "#one", "title": "Parent", "assignee": "lead", "repo": "repo-one"}),
    )
    .await?;
    let parent = parent["task"]["id"].as_str().unwrap();
    let (_, child) = delegation_post(&api, &tokens["lead"], "/v1/tasks",
        json!({"channel": "#one", "title": "Child @pm @here", "assignee": "worker", "parent_task_id": parent})).await?;
    let child = child["task"]["id"].as_str().unwrap();
    let (_, leased) = delegation_post(
        &api,
        &tokens["lead"],
        "/v1/tasks",
        json!({"channel": "#one", "title": "Leased", "parent_task_id": parent}),
    )
    .await?;
    let leased = leased["task"]["id"].as_str().unwrap();
    assert_eq!(
        delegation_post(
            &api,
            &tokens["peer"],
            &format!("/v1/tasks/{leased}/claim"),
            json!({})
        )
        .await?
        .0,
        200
    );
    for nick in ["worker", "peer"] {
        api.server.wakes().upsert(
            nick,
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
                timeout_seconds: Some(10),
                max_runs_per_hour: Some(100),
                ..Default::default()
            },
        )?;
    }
    assert_eq!(
        delegation_post(
            &api,
            &tokens["lead"],
            &format!("/v1/tasks/{parent}/ask"),
            json!({"question": "Pause the tree"})
        )
        .await?
        .0,
        200
    );
    // The former lease holder needs the stop notice even after its lease is cleared.
    for _ in 0..250 {
        let runs = api.server.wakes().runs(None, 20)?;
        if runs.len() == 2 && runs.iter().all(|r| r.status == "ok") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let runs = api.server.wakes().runs(None, 20)?;
    assert_eq!(runs.len(), 2, "{runs:?}");
    for (nick, id) in [("worker", child), ("peer", leased)] {
        let run = runs.iter().find(|r| r.principal_id == nick).unwrap();
        assert_eq!(run.status, "ok", "{run:?}");
        assert_eq!(run.task_id.as_deref(), Some(id));
        assert_eq!(run.channel, "#one-s1");
        assert!(run.log_tail.as_deref().unwrap().contains("DELEGATION "));
    }
    assert_eq!(
        delegation_post(
            &api,
            &tokens["pm"],
            &format!("/v1/tasks/{parent}/answer"),
            json!({"answer": "Continue"})
        )
        .await?
        .0,
        200
    );
    expect_delegation_wake(&api, 3, "worker", child).await?;
    assert_eq!(api.server.get_task_api(leased)?.status, "open");
    // An open task has no worker to resume; cancellation still notifies its assigned sibling.
    assert_eq!(
        delegation_post(
            &api,
            &tokens["pm"],
            &format!("/v1/tasks/{parent}/status"),
            json!({"status": "cancelled"})
        )
        .await?
        .0,
        200
    );
    expect_delegation_wake(&api, 4, "worker", child).await?;
    let notices = api.server.history().tail("#one-s1", 0, 100)?;
    assert_eq!(
        notices
            .iter()
            .filter(|m| m.body.contains("because ancestor"))
            .count(),
        6
    );
    Ok(())
}
