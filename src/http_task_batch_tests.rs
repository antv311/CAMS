async fn batch_create(api: &TemplateApi, token: &str, tasks: Value) -> Result<(u16, Value)> {
    delegation_post(api, token, "/v1/tasks/batch", json!({"tasks": tasks})).await
}

async fn batch_status(api: &TemplateApi, token: &str, updates: Value) -> Result<(u16, Value)> {
    delegation_post(
        api,
        token,
        "/v1/tasks/bulk-status",
        json!({"updates": updates}),
    )
    .await
}

fn batch_task_state(api: &TemplateApi) -> Result<Value> {
    let tasks = api.server.tasks().list_all(None, None, true)?;
    let events = tasks
        .iter()
        .map(|task| api.server.tasks().events(&task.id))
        .collect::<Result<Vec<_>>>()?;
    let metrics = api
        .server
        .metrics()
        .render_prometheus(&api.server.metrics_snapshot()?);
    let counters: Vec<_> = metrics
        .lines()
        .filter(|line| {
            line.starts_with("aircd_tasks_created_total")
                || line.starts_with("aircd_tasks_completed_total")
                || line.starts_with("aircd_task_transitions_total")
                || line.starts_with("aircd_task_events_persisted_total")
        })
        .collect();
    Ok(
        json!({"tasks": tasks, "events": events, "counters": counters, "messages": api.server.history().tail("#one", 0, 1000)?}),
    )
}

#[tokio::test]
async fn task_batch_creates_forward_dependencies_and_parents_with_inherited_fields() -> Result<()> {
    let (api, tokens) = task_workflow_api().await?;
    let owner = api.server.auth().resolve_by_name("human")?.unwrap();
    let room = api.server.open_room_api(&owner, "#one", None)?;
    api.server.gits().upsert(
        "backend",
        "#one",
        &std::env::current_dir()?.to_string_lossy(),
        None,
        None,
        None,
    )?;
    let (status, result) = batch_create(&api, &tokens["pm"], json!([
        {"key":"child", "channel":"#one", "title":" UI ", "assignee":"worker", "parent_task_id":"$parent", "blocked_by":["$api", "$api"], "timeout_seconds":30},
        {"key":"api", "channel":room.channel, "title":"API", "assignee":"peer", "priority":"HIGH", "kind":"feature"},
        {"key":"parent", "channel":room.channel, "title":"Parent", "assignee":"lead", "repo":"backend"}
    ])).await?;
    assert_eq!(status, 201, "{result}");
    let tasks = result["tasks"].as_array().unwrap();
    let ids = &result["task_ids"];
    assert_eq!(tasks.len(), 3);
    assert_eq!(tasks[0]["id"], ids["child"]);
    assert_eq!(tasks[0]["title"], "UI");
    assert_eq!(tasks[0]["parent_task_id"], ids["parent"]);
    assert_eq!(tasks[0]["blocked_by"], json!([ids["api"]]));
    assert_eq!(tasks[0]["repo"], "backend");
    assert_eq!(tasks[0]["sprint"], room.channel);
    assert_eq!(tasks[0]["reviewer"], "lead");
    assert_eq!(tasks[0]["created_by"], "pm");
    assert_eq!(tasks[0]["timeout_seconds"], 30);
    assert_eq!(tasks[1]["priority"], "high");
    assert_eq!(tasks[1]["reviewer"], "pm");
    assert_eq!(
        api.server.tasks().events(ids["child"].as_str().unwrap())?[0].action,
        "dependencies"
    );
    let history = api.server.history().tail(&room.channel, 0, 100)?;
    assert_eq!(
        history
            .iter()
            .filter(|m| m.body.contains("TASK ") && m.body.contains("created by"))
            .count(),
        3
    );
    Ok(())
}

#[tokio::test]
async fn task_batch_rolls_back_late_storage_validation_and_dependency_graph_errors() -> Result<()> {
    let (api, tokens) = task_workflow_api().await?;
    let before = batch_task_state(&api)?;
    for tasks in [
        json!([{"channel":"#one", "title":"Good"}, {"channel":"#one", "title":"Bad", "priority":"invalid"}]),
        json!([{"channel":"#one", "title":"Good"}, {"channel":"#one", "title":"Bad", "timeout_seconds":0}]),
        json!([{"channel":"#one", "title":"Good"}, {"channel":"#one", "title":"Bad", "blocked_by":["task_missing"]}]),
        json!([{"key":"a", "channel":"#one", "title":"A", "blocked_by":["$b"]}, {"key":"b", "channel":"#one", "title":"B", "blocked_by":["$a"]}]),
        json!([{"key":"a", "channel":"#one", "title":"A", "blocked_by":["$a"]}]),
        json!([{"key":"a", "channel":"#one", "title":"A", "parent_task_id":"$b"}, {"key":"b", "channel":"#one", "title":"B", "parent_task_id":"$a"}]),
    ] {
        let (status, body) = batch_create(&api, &tokens["pm"], tasks).await?;
        assert_eq!(status, 400, "{body}");
        assert_eq!(batch_task_state(&api)?, before, "{body}");
    }
    Ok(())
}

#[tokio::test]
async fn task_batch_validates_shape_bounds_keys_and_all_item_permissions() -> Result<()> {
    let (api, tokens) = task_workflow_api().await?;
    let before = batch_task_state(&api)?;
    for tasks in [
        json!([]),
        json!(vec![json!({"channel":"#one", "title":"Too many"}); 101]),
        json!([{"channel":"#one", "title":"Good"}, {"title":"Missing channel"}]),
        json!([{"channel":"#one", "title":"Good"}, {"channel":"#one", "title":" "}]),
        json!([{"key":"x", "channel":"#one", "title":"A"}, {"key":"x", "channel":"#one", "title":"B"}]),
        json!([{"key":"$bad", "channel":"#one", "title":"A"}]),
        json!([{"key":"", "channel":"#one", "title":"A"}]),
        json!([{"channel":"#one", "title":"A", "blocked_by":["$missing"]}]),
        json!([{"channel":"#one", "title":"Good"}, {"channel":"#one", "title":"Bad", "repo":"missing"}]),
        json!([{"channel":"#one", "title":"Good"}, {"channel":"#one", "title":"Bad", "assignee":"missing"}]),
        json!([{"channel":"#one", "title":"Good"}, {"channel":"#one", "title":"Bad", "reviewer":"missing"}]),
    ] {
        let (status, body) = batch_create(&api, &tokens["pm"], tasks).await?;
        assert_eq!(status, 400, "{body}");
        assert_eq!(batch_task_state(&api)?, before);
    }
    let (status, body) = batch_create(
        &api,
        &tokens["worker"],
        json!([
            {"channel":"#one", "title":"Self", "assignee":"worker"},
            {"channel":"#one", "title":"Forbidden", "assignee":"peer"}
        ]),
    )
    .await?;
    assert_eq!(status, 400, "{body}");
    assert_eq!(batch_task_state(&api)?, before);
    let (status, body) = batch_create(
        &api,
        &tokens["pm"],
        json!(vec![json!({"channel":"#one", "title":"At limit"}); 100]),
    )
    .await?;
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["tasks"].as_array().unwrap().len(), 100);
    let updates: Vec<_> = body["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|task| json!({"task_id":task["id"], "status":"cancelled"}))
        .collect();
    let (status, body) = batch_status(&api, &tokens["pm"], json!(updates)).await?;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["tasks"].as_array().unwrap().len(), 100);
    Ok(())
}

#[tokio::test]
async fn task_batch_authentication_scopes_and_malformed_payloads_have_no_effect() -> Result<()> {
    let (api, tokens) = task_workflow_api().await?;
    let readonly = api.server.auth().issue_token_with_options(
        "pm",
        None,
        false,
        Some(&["read".into()]),
        None,
    )?;
    let write = api.server.auth().issue_token_with_options(
        "pm",
        None,
        false,
        Some(&["write".into()]),
        None,
    )?;
    let before = batch_task_state(&api)?;
    for (path, body) in [
        (
            "/v1/tasks/batch",
            json!({"tasks":[{"channel":"#one", "title":"A"}]}),
        ),
        (
            "/v1/tasks/bulk-status",
            json!({"updates":[{"task_id":"missing", "status":"cancelled"}]}),
        ),
    ] {
        for token in [None, Some("invalid")] {
            assert_eq!(
                api.request("POST", path, token, None, Some(&body)).await?.0,
                401
            );
        }
        for token in [&readonly.token, &write.token] {
            assert_eq!(
                delegation_post(&api, token, path, body.clone()).await?.0,
                403
            );
        }
    }
    for (path, body) in [
        ("/v1/tasks/batch", json!({"tasks":"wrong"})),
        (
            "/v1/tasks/batch",
            json!({"tasks":[{"channel":"#one", "title":"A", "timeout_seconds":-1}]}),
        ),
        (
            "/v1/tasks/bulk-status",
            json!({"updates":[{"task_id":"a", "status":"testing", "unknown":true}]}),
        ),
    ] {
        let (status, _) = delegation_post(&api, &tokens["pm"], path, body).await?;
        assert!((400..500).contains(&status));
    }
    assert_eq!(batch_task_state(&api)?, before);
    Ok(())
}

async fn make_batch_pair(
    api: &TemplateApi,
    token: &str,
    assignee: &str,
) -> Result<(String, String)> {
    let (status, body) = batch_create(
        api,
        token,
        json!([
            {"key":"a", "channel":"#one", "title":"A", "assignee":assignee},
            {"key":"b", "channel":"#one", "title":"B", "assignee":assignee}
        ]),
    )
    .await?;
    assert_eq!(status, 201, "{body}");
    Ok((
        body["task_ids"]["a"].as_str().unwrap().into(),
        body["task_ids"]["b"].as_str().unwrap().into(),
    ))
}

#[tokio::test]
async fn task_batch_bulk_status_requires_evidence_and_preserves_single_task_authorization(
) -> Result<()> {
    let (api, tokens) = task_workflow_api().await?;
    let (a, b) = make_batch_pair(&api, &tokens["pm"], "worker").await?;
    let write = api.server.auth().issue_token_with_options(
        "worker",
        None,
        false,
        Some(&["write".into()]),
        None,
    )?;
    let before = batch_task_state(&api)?;
    assert_eq!(
        batch_status(
            &api,
            &tokens["peer"],
            json!([
                {"task_id":a, "status":"in_progress"}, {"task_id":b, "status":"testing"}
            ])
        )
        .await?
        .0,
        400
    );
    assert_eq!(batch_task_state(&api)?, before);
    for to in ["in-progress", "testing"] {
        let (status, body) = batch_status(
            &api,
            &write.token,
            json!([
                {"task_id":a, "status":to}, {"task_id":b, "status":to}
            ]),
        )
        .await?;
        assert_eq!(status, 200, "{body}");
    }
    let before = batch_task_state(&api)?;
    let (status, body) = batch_status(
        &api,
        &write.token,
        json!([
            {"task_id":a, "status":"review", "note":"tests passed", "branch":"feat/a"},
            {"task_id":b, "status":"review"}
        ]),
    )
    .await?;
    assert_eq!(status, 400, "{body}");
    assert!(body["error"].as_str().unwrap().contains("test report"));
    assert_eq!(batch_task_state(&api)?, before);
    let (status, body) = batch_status(
        &api,
        &write.token,
        json!([
            {"task_id":a, "status":"review", "note":"A passed", "branch":"feat/a"},
            {"task_id":b, "status":"review", "report_body":"B passed", "branch":"feat/b"}
        ]),
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    for id in [&a, &b] {
        assert!(api
            .server
            .tasks()
            .events(id)?
            .iter()
            .any(|event| event.to_status.as_deref() == Some("review")
                && event
                    .note
                    .as_deref()
                    .is_some_and(|note| note.contains("passed"))));
    }
    assert_eq!(
        batch_status(
            &api,
            &write.token,
            json!([
                {"task_id":a, "status":"merged"}, {"task_id":b, "status":"merged"}
            ])
        )
        .await?
        .0,
        403
    );
    assert_eq!(
        batch_status(
            &api,
            &tokens["pm"],
            json!([
                {"task_id":a, "status":"merged"}, {"task_id":b, "status":"merged"}
            ])
        )
        .await?
        .0,
        200
    );
    Ok(())
}

#[tokio::test]
async fn task_batch_bulk_status_orders_dependencies_and_rolls_back_late_blockers() -> Result<()> {
    let (api, tokens) = task_workflow_api().await?;
    let (a, b) = make_batch_pair(&api, &tokens["pm"], "pm").await?;
    let pm = api.server.auth().resolve_by_name("pm")?.unwrap();
    api.server
        .add_task_dependencies_api(&pm, &b, std::slice::from_ref(&a))?;
    for status in ["testing", "review"] {
        api.server
            .set_task_status_api(&pm, &a, status, Some("tests passed"), None, None, None)?;
    }
    let before = batch_task_state(&api)?;
    // First mutation would succeed; second is still blocked and must roll it back.
    assert_eq!(
        batch_status(
            &api,
            &tokens["pm"],
            json!([
                {"task_id":a, "status":"changes_requested"}, {"task_id":b, "status":"in_progress"}
            ])
        )
        .await?
        .0,
        400
    );
    assert_eq!(batch_task_state(&api)?, before);
    // Request order matters: a later resolution cannot authorize an earlier start.
    assert_eq!(
        batch_status(
            &api,
            &tokens["pm"],
            json!([
                {"task_id":b, "status":"in_progress"}, {"task_id":a, "status":"merged"}
            ])
        )
        .await?
        .0,
        400
    );
    assert_eq!(batch_task_state(&api)?, before);
    let (status, body) = batch_status(
        &api,
        &tokens["pm"],
        json!([
            {"task_id":a, "status":"merged"}, {"task_id":b, "status":"in_progress"}
        ]),
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["tasks"][0]["status"], "merged");
    assert_eq!(body["tasks"][1]["status"], "in_progress");
    Ok(())
}

#[tokio::test]
async fn task_batch_bulk_rejects_duplicates_missing_tasks_and_invalid_transitions_atomically(
) -> Result<()> {
    let (api, tokens) = task_workflow_api().await?;
    let (a, b) = make_batch_pair(&api, &tokens["pm"], "worker").await?;
    let before = batch_task_state(&api)?;
    for updates in [
        json!([]),
        json!(vec![json!({"task_id":a, "status":"testing"}); 101]),
        json!([{"task_id":a, "status":"testing"}, {"task_id":format!(" {a} "), "status":"testing"}]),
        json!([{"task_id":a, "status":"testing"}, {"task_id":"missing", "status":"testing"}]),
        json!([{"task_id":a, "status":"testing"}, {"task_id":b, "status":"review", "note":"tests passed"}]),
        json!([{"task_id":a, "status":"testing"}, {"task_id":b, "status":"unknown"}]),
        json!([{"task_id":a, "status":"testing"}, {"task_id":" ", "status":"testing"}]),
    ] {
        let (status, body) = batch_status(&api, &tokens["worker"], updates).await?;
        assert_eq!(status, 400, "{body}");
        assert_eq!(batch_task_state(&api)?, before);
    }
    Ok(())
}

#[tokio::test]
async fn task_batch_cascade_cleanup_is_atomic_and_rejects_explicit_overlap() -> Result<()> {
    let (api, tokens) = task_workflow_api().await?;
    let (_, body) = batch_create(&api, &tokens["pm"], json!([
        {"key":"p", "channel":"#one", "title":"Parent", "assignee":"lead"},
        {"key":"c", "channel":"#one", "title":"Child", "assignee":"worker", "parent_task_id":"$p"}
    ])).await?;
    let parent = body["task_ids"]["p"].as_str().unwrap();
    let child = body["task_ids"]["c"].as_str().unwrap();
    let worker = api.server.auth().resolve_by_name("worker")?.unwrap();
    api.server.ask_task_api(&worker, child, "Need decision")?;
    let before = batch_task_state(&api)?;
    for updates in [
        json!([{"task_id":parent, "status":"cancelled"}, {"task_id":child, "status":"cancelled"}]),
        json!([{"task_id":child, "status":"cancelled"}, {"task_id":parent, "status":"cancelled"}]),
    ] {
        assert_eq!(batch_status(&api, &tokens["pm"], updates).await?.0, 400);
        assert_eq!(batch_task_state(&api)?, before);
        assert!(api.server.tasks().pending_question(child)?.is_some());
    }
    let (status, result) = batch_status(
        &api,
        &tokens["pm"],
        json!([
            {"task_id":parent, "status":"cancelled", "note":"Scope removed"}
        ]),
    )
    .await?;
    assert_eq!(status, 200, "{result}");
    assert_eq!(api.server.get_task_api(child)?.status, "cancelled");
    assert!(api.server.tasks().pending_question(child)?.is_none());
    assert!(api
        .server
        .tasks()
        .events(child)?
        .iter()
        .any(|event| event.action == "parent_cancel"));
    Ok(())
}

#[tokio::test]
async fn task_batch_concurrent_bulk_requests_commit_only_one_transition_per_task() -> Result<()> {
    let (api, tokens) = task_workflow_api().await?;
    let (a, b) = make_batch_pair(&api, &tokens["pm"], "worker").await?;
    let updates = json!([{"task_id":a, "status":"testing"}, {"task_id":b, "status":"testing"}]);
    let (first, second) = tokio::join!(
        batch_status(&api, &tokens["worker"], updates.clone()),
        batch_status(&api, &tokens["worker"], updates)
    );
    let mut codes = vec![first?.0, second?.0];
    codes.sort();
    assert_eq!(codes, vec![200, 400]);
    for id in [&a, &b] {
        assert_eq!(api.server.get_task_api(id)?.status, "testing");
        assert_eq!(
            api.server
                .tasks()
                .events(id)?
                .iter()
                .filter(|event| event.action == "status")
                .count(),
            1
        );
    }
    Ok(())
}

#[tokio::test]
async fn task_batch_review_rounds_and_manager_bypasses_remain_revision_bound() -> Result<()> {
    let (api, tokens) = consensus_api(None).await?;
    let (a, b) = make_batch_pair(&api, &tokens["pm"], "worker").await?;
    assert_eq!(
        batch_status(
            &api,
            &tokens["worker"],
            json!([
                {"task_id":a, "status":"testing"}, {"task_id":b, "status":"testing"}
            ])
        )
        .await?
        .0,
        200
    );
    let before = batch_task_state(&api)?;
    assert_eq!(batch_status(&api, &tokens["worker"], json!([
        {"task_id":a, "status":"review", "note":"tests passed", "branch":"feat/a", "commit":CONSENSUS_COMMIT},
        {"task_id":b, "status":"review", "note":"tests passed", "branch":"feat/b"}
    ])).await?.0, 400);
    assert_eq!(batch_task_state(&api)?, before);
    let (status, body) = batch_status(&api, &tokens["worker"], json!([
        {"task_id":a, "status":"review", "note":"tests passed", "branch":"feat/a", "commit":CONSENSUS_COMMIT},
        {"task_id":b, "status":"review", "note":"tests passed", "branch":"feat/b", "commit":CONSENSUS_COMMIT}
    ])).await?;
    assert_eq!(status, 200, "{body}");
    let first = api.server.get_task_api(&a)?.review_round.unwrap();
    let second = api.server.get_task_api(&b)?.review_round.unwrap();
    assert_ne!(first.revision, second.revision);
    let before = batch_task_state(&api)?;
    // An otherwise valid first manager bypass must roll back if the second
    // item omits its own required explanation.
    assert_eq!(
        batch_status(
            &api,
            &tokens["pm"],
            json!([
                {"task_id":a, "status":"merged", "note":"Owner schedule requires bypass"},
                {"task_id":b, "status":"merged"}
            ])
        )
        .await?
        .0,
        400
    );
    assert_eq!(batch_task_state(&api)?, before);
    assert_eq!(
        batch_status(
            &api,
            &tokens["pm"],
            json!([
                {"task_id":a, "status":"merged", "note":"Accepted with bypass"},
                {"task_id":b, "status":"merged", "note":"Accepted with bypass"}
            ])
        )
        .await?
        .0,
        200
    );
    for id in [&a, &b] {
        assert!(api
            .server
            .tasks()
            .events(id)?
            .last()
            .unwrap()
            .note
            .as_deref()
            .unwrap()
            .contains("consensus bypass"));
    }
    Ok(())
}

#[tokio::test]
async fn task_batch_expired_execution_rejects_and_rolls_back_earlier_work() -> Result<()> {
    let (api, tokens) = task_workflow_api().await?;
    let (_, body) = batch_create(
        &api,
        &tokens["pm"],
        json!([
            {"channel":"#one", "title":"Ordinary", "assignee":"worker"},
            {"channel":"#one", "title":"Timed", "assignee":"worker", "timeout_seconds":1}
        ]),
    )
    .await?;
    let a = body["tasks"][0]["id"].as_str().unwrap();
    let b = body["tasks"][1]["id"].as_str().unwrap();
    assert_eq!(
        batch_status(
            &api,
            &tokens["worker"],
            json!([
                {"task_id":a, "status":"in_progress"}, {"task_id":b, "status":"in_progress"}
            ])
        )
        .await?
        .0,
        200
    );
    tokio::time::sleep(std::time::Duration::from_millis(2100)).await;
    let before = batch_task_state(&api)?;
    let (status, body) = batch_status(
        &api,
        &tokens["worker"],
        json!([
            {"task_id":a, "status":"testing"}, {"task_id":b, "status":"testing"}
        ]),
    )
    .await?;
    assert_eq!(status, 400, "{body}");
    assert!(body["error"].as_str().unwrap().contains("timed out"));
    assert_eq!(batch_task_state(&api)?, before);
    Ok(())
}
