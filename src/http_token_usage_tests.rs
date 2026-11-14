// Authenticated HTTP coverage of task token accounting: usage reports, subtask
// rollup, budgets set by a PM or parent lead, and the export metrics.

#[tokio::test]
async fn task_token_usage_rolls_up_and_enforces_budgets() -> Result<()> {
    let (api, tokens) = task_workflow_api().await?;
    let (status, body) = delegation_post(
        &api,
        &tokens["pm"],
        "/v1/tasks",
        json!({"channel": "#one", "title": "Parent", "assignee": "lead"}),
    )
    .await?;
    assert_eq!(status, 201, "{body}");
    let parent = body["task"]["id"].as_str().unwrap().to_string();
    let (status, body) = delegation_post(
        &api,
        &tokens["lead"],
        "/v1/tasks",
        json!({"channel": "#one", "title": "Child", "assignee": "worker",
               "parent_task_id": parent}),
    )
    .await?;
    assert_eq!(status, 201, "{body}");
    let child = body["task"]["id"].as_str().unwrap().to_string();

    // Only a PM or the parent's lead may set budgets.
    let (status, body) = delegation_post(
        &api,
        &tokens["worker"],
        &format!("/v1/tasks/{child}/budget"),
        json!({"token_budget": 10}),
    )
    .await?;
    assert_eq!(status, 403, "{body}");
    let (status, body) = delegation_post(
        &api,
        &tokens["pm"],
        &format!("/v1/tasks/{parent}/budget"),
        json!({"token_budget": 1000}),
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["usage"]["token_budget"], 1000);
    assert_eq!(body["usage"]["remaining"], 1000);
    let (status, body) = delegation_post(
        &api,
        &tokens["lead"],
        &format!("/v1/tasks/{child}/budget"),
        json!({"token_budget": 600}),
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    for bad in [
        json!({}),
        json!({"token_budget": -5}),
        json!({"token_budget": "x"}),
    ] {
        let (status, body) = delegation_post(
            &api,
            &tokens["pm"],
            &format!("/v1/tasks/{child}/budget"),
            bad,
        )
        .await?;
        assert_eq!(status, 400, "{body}");
    }

    // The worker reports turns; an outsider cannot.
    let (status, body) = delegation_post(
        &api,
        &tokens["worker"],
        &format!("/v1/tasks/{child}/usage"),
        json!({"prompt_tokens": 300, "completion_tokens": 100,
               "session_id": "sess-1", "model": "gemma"}),
    )
    .await?;
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["recorded"]["total_tokens"], 400);
    assert_eq!(body["usage"]["remaining"], 200);
    assert_eq!(body["usage"]["limited_by"], child.as_str());
    assert_eq!(body["usage"]["exceeded"], false);
    let (_, outsider_token) =
        api.server
            .auth()
            .register_principal("outsider", Some("builder"), None)?;
    let (status, body) = delegation_post(
        &api,
        &outsider_token,
        &format!("/v1/tasks/{child}/usage"),
        json!({"prompt_tokens": 1}),
    )
    .await?;
    assert_eq!(status, 403, "{body}");
    let (status, body) = delegation_post(
        &api,
        &tokens["worker"],
        &format!("/v1/tasks/{child}/usage"),
        json!({"prompt_tokens": -1}),
    )
    .await?;
    assert_eq!(status, 400, "{body}");

    // Lead spends on the parent; the parent's budget now limits the child.
    let (status, body) = delegation_post(
        &api,
        &tokens["lead"],
        &format!("/v1/tasks/{parent}/usage"),
        json!({"prompt_tokens": 450, "completion_tokens": 50, "session_id": "lead-1"}),
    )
    .await?;
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["usage"]["own"]["total_tokens"], 500);
    assert_eq!(body["usage"]["subtasks"]["total_tokens"], 400);
    assert_eq!(body["usage"]["remaining"], 100);

    let (status, body) = delegation_post(
        &api,
        &tokens["worker"],
        &format!("/v1/tasks/{child}/usage"),
        json!({"prompt_tokens": 100, "completion_tokens": 50, "total_tokens": 160,
               "session_id": "sess-1", "model": "gemma"}),
    )
    .await?;
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["usage"]["exceeded"], true);
    assert_eq!(body["usage"]["limited_by"], parent.as_str());
    assert_eq!(body["usage"]["remaining"], -60);

    let (status, body) = call(
        &api.client,
        "GET",
        format!("{}/v1/tasks/{child}/usage", api.base),
        &tokens["peer"],
        json!({}),
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    let sessions = body["usage"]["by_session"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["records"], 2);
    assert_eq!(sessions[0]["total_tokens"], 560);
    assert_eq!(sessions[0]["recorded_by"], "worker");

    let (status, body) = call(
        &api.client,
        "GET",
        format!("{}/v1/tasks/{parent}", api.base),
        &tokens["peer"],
        json!({}),
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["tokens"]["total"]["total_tokens"], 1060);
    assert_eq!(body["tokens"]["by_subtask"][0]["task_id"], child.as_str());
    let events = body["events"].as_array().unwrap();
    assert!(events.iter().any(|e| e["action"] == "token_budget"));
    assert_eq!(
        events
            .iter()
            .filter(|e| e["action"] == "token_budget_exceeded")
            .count(),
        1
    );

    let (status, body) = call(
        &api.client,
        "GET",
        format!("{}/v1/tasks/export?channel=%23one&events=false", api.base),
        &tokens["peer"],
        json!({}),
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["token_totals"]["total_tokens"], 1060);
    let rows = body["tasks"].as_array().unwrap();
    let row = rows
        .iter()
        .find(|r| r["task"]["id"] == parent.as_str())
        .unwrap();
    assert_eq!(row["metrics"]["tokens"]["own"]["total_tokens"], 500);
    assert_eq!(row["metrics"]["tokens"]["total"]["total_tokens"], 1060);
    assert_eq!(row["metrics"]["tokens"]["exceeded"], true);

    // Clearing the parent budget leaves only the child's own budget (560 of 600 spent).
    let (status, body) = delegation_post(
        &api,
        &tokens["pm"],
        &format!("/v1/tasks/{parent}/budget"),
        json!({"token_budget": null}),
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["usage"]["token_budget"], Value::Null);
    let report = api.server.token_usage().report(&child)?;
    assert_eq!(report.limited_by.as_deref(), Some(child.as_str()));
    assert!(!report.exceeded);
    assert_eq!(report.remaining, Some(40));

    let (status, _) = call(
        &api.client,
        "GET",
        format!("{}/v1/tasks/task_missing/usage", api.base),
        &tokens["peer"],
        json!({}),
    )
    .await?;
    assert_eq!(status, 404);
    Ok(())
}
