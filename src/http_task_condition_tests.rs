async fn condition_workflow(
    api: &TemplateApi,
    token: &str,
    id: &str,
    status: &str,
    output: Option<Value>,
) -> Result<Value> {
    let mut body = json!({"status":status, "note":"workflow regression checks passed"});
    if let Some(output) = output {
        body["output"] = output;
    }
    let (code, body) = delegation_post(api, token, &format!("/v1/tasks/{id}/status"), body).await?;
    assert_eq!(code, 200, "{body}");
    Ok(body["task"].clone())
}

async fn condition_parent(api: &TemplateApi, token: &str) -> Result<String> {
    let (code, body) = delegation_post(
        api,
        token,
        "/v1/tasks",
        json!({"channel":"#one", "title":"Parent", "assignee":"lead"}),
    )
    .await?;
    assert_eq!(code, 201, "{body}");
    Ok(body["task"]["id"].as_str().unwrap().into())
}

#[tokio::test]
async fn conditional_parent_outputs_choose_branches_and_skip_descendants_atomically() -> Result<()>
{
    let (api, tokens) = task_workflow_api().await?;
    let parent = condition_parent(&api, &tokens["pm"]).await?;
    let (code, body) = batch_create(&api, &tokens["pm"], json!([
        {"key":"yes", "channel":"#one", "title":"Fast", "assignee":"worker", "parent_task_id":parent,
            "condition":{"type":"parent_output_equals", "pointer":"/route", "value":"fast"}},
        {"key":"no", "channel":"#one", "title":"Slow", "assignee":"peer", "parent_task_id":parent,
            "condition":{"type":"parent_output_equals", "pointer":"/route", "value":"slow"}},
        {"key":"exists", "channel":"#one", "title":"Escaped key", "assignee":"worker", "parent_task_id":parent,
            "condition":{"type":"parent_output_exists", "pointer":"/a~1b/~0key"}},
        {"key":"missing", "channel":"#one", "title":"Missing", "parent_task_id":parent,
            "condition":{"type":"parent_output_exists", "pointer":"/missing"}},
        {"key":"nested", "channel":"#one", "title":"Nested false", "assignee":"worker", "parent_task_id":"$no"},
        {"key":"nested_yes", "channel":"#one", "title":"Nested true", "assignee":"peer", "parent_task_id":"$yes"}
    ])).await?;
    assert_eq!(code, 201, "{body}");
    let ids = &body["task_ids"];
    for task in body["tasks"].as_array().unwrap() {
        assert_eq!(task["status"], "dormant");
    }
    let worker = api.server.auth().resolve_by_name("worker")?.unwrap();
    let inbox = api.server.inbox_api(&worker)?;
    assert_eq!(inbox["ready_task_count"], 0);
    let dormant = api.server.get_task_api(ids["yes"].as_str().unwrap())?;
    assert!(api
        .server
        .task_wake_recipients(&dormant, "create")?
        .is_empty());
    condition_workflow(&api, &tokens["lead"], &parent, "testing", None).await?;
    condition_workflow(&api, &tokens["lead"], &parent, "review", None).await?;
    let output = json!({"route":"fast", "a/b":{"~key":null}});
    let merged =
        condition_workflow(&api, &tokens["pm"], &parent, "merged", Some(output.clone())).await?;
    assert_eq!(merged["output"], output);
    for (key, status) in [
        ("yes", "assigned"),
        ("exists", "assigned"),
        ("no", "skipped"),
        ("missing", "skipped"),
        ("nested", "skipped"),
        ("nested_yes", "assigned"),
    ] {
        assert_eq!(
            api.server.get_task_api(ids[key].as_str().unwrap())?.status,
            status,
            "{key}"
        );
    }
    assert_eq!(api.server.inbox_api(&worker)?["ready_task_count"], 2);
    let events = api.server.tasks().events(ids["no"].as_str().unwrap())?;
    assert_eq!(events.last().unwrap().action, "condition_skip");
    let messages = api.server.history().tail("#one", 0, 1000)?;
    let skip_message = messages
        .iter()
        .find(|m| m.body.contains(ids["no"].as_str().unwrap()) && m.body.contains("condition_skip"))
        .unwrap();
    assert!(
        !skip_message.body.contains("cc @"),
        "false branches must not dispatch a worker"
    );
    assert!(api
        .server
        .tasks()
        .list("#one")?
        .iter()
        .all(|t| t.status != "skipped"));
    let (code, _) = delegation_post(
        &api,
        &tokens["pm"],
        &format!("/v1/tasks/{parent}/status"),
        json!({"status":"merged", "output":{"route":"slow"}}),
    )
    .await?;
    assert_eq!(code, 400);
    assert_eq!(api.server.get_task_api(&parent)?.output, Some(output));
    Ok(())
}

#[tokio::test]
async fn conditional_dormant_tasks_reject_execution_and_reassignment_preserves_gates() -> Result<()>
{
    let (api, tokens) = task_workflow_api().await?;
    let parent = condition_parent(&api, &tokens["pm"]).await?;
    let (_, result) = batch_create(&api, &tokens["pm"], json!([
        {"key":"assigned", "channel":"#one", "title":"Optional", "assignee":"worker", "parent_task_id":parent, "optional":true},
        {"key":"open", "channel":"#one", "title":"Optional pool", "parent_task_id":parent, "optional":true}
    ])).await?;
    let assigned = result["task_ids"]["assigned"].as_str().unwrap();
    let open = result["task_ids"]["open"].as_str().unwrap();
    let before = batch_task_state(&api)?;
    for id in [assigned, open] {
        for (action, body) in [
            ("claim", json!({})),
            ("done", json!({"output":{}})),
            ("release", json!({})),
            ("status", json!({"status":"in_progress"})),
            ("status", json!({"status":"testing"})),
            ("ask", json!({"question":"bypass"})),
            ("heartbeat", json!({})),
            ("enable", json!({})),
        ] {
            let (code, _) = delegation_post(
                &api,
                &tokens["worker"],
                &format!("/v1/tasks/{id}/{action}"),
                body,
            )
            .await?;
            assert!((400..500).contains(&code), "{action}: {code}");
        }
    }
    assert_eq!(batch_task_state(&api)?, before);
    let (code, result) = delegation_post(
        &api,
        &tokens["pm"],
        &format!("/v1/tasks/{assigned}/assign"),
        json!({"assignee":"peer"}),
    )
    .await?;
    assert_eq!(code, 200, "{result}");
    assert_eq!(result["task"]["status"], "dormant");
    let (code, result) = delegation_post(
        &api,
        &tokens["lead"],
        &format!("/v1/tasks/{assigned}/enable"),
        json!({}),
    )
    .await?;
    assert_eq!(code, 200, "{result}");
    assert_eq!(result["task"]["status"], "assigned");
    assert_eq!(result["task"]["optional_enabled"], true);
    let (code, result) = delegation_post(
        &api,
        &tokens["pm"],
        &format!("/v1/tasks/{open}/enable"),
        json!({}),
    )
    .await?;
    assert_eq!(code, 200, "{result}");
    assert_eq!(result["task"]["status"], "open");
    let (code, _) = delegation_post(
        &api,
        &tokens["worker"],
        &format!("/v1/tasks/{open}/claim"),
        json!({}),
    )
    .await?;
    assert_eq!(code, 200);
    Ok(())
}

#[tokio::test]
async fn conditional_sibling_success_optional_enable_and_failed_sibling_propagation() -> Result<()>
{
    let (api, tokens) = task_workflow_api().await?;
    let parent = condition_parent(&api, &tokens["pm"]).await?;
    let (code, result) = batch_create(&api, &tokens["pm"], json!([
        {"key":"next", "channel":"#one", "title":"Next", "assignee":"worker", "parent_task_id":parent, "optional":true,
            "condition":{"type":"sibling_succeeded", "task_id":"$first"}},
        {"key":"first", "channel":"#one", "title":"First", "assignee":"worker", "parent_task_id":parent},
        {"key":"after_no", "channel":"#one", "title":"After skip", "parent_task_id":parent,
            "condition":{"type":"sibling_succeeded", "task_id":"$no"}},
        {"key":"no", "channel":"#one", "title":"After cancellation", "parent_task_id":parent,
            "condition":{"type":"sibling_succeeded", "task_id":"$cancel"}},
        {"key":"cancel", "channel":"#one", "title":"Cancel", "parent_task_id":parent}
    ])).await?;
    assert_eq!(code, 201, "{result}");
    let ids = &result["task_ids"];
    let first = ids["first"].as_str().unwrap();
    let next = ids["next"].as_str().unwrap();
    assert_eq!(result["tasks"][0]["condition"]["task_id"], first);
    condition_workflow(&api, &tokens["worker"], first, "testing", None).await?;
    condition_workflow(&api, &tokens["worker"], first, "review", None).await?;
    condition_workflow(&api, &tokens["lead"], first, "merged", None).await?;
    assert_eq!(api.server.get_task_api(next)?.status, "dormant");
    let (code, body) = delegation_post(
        &api,
        &tokens["pm"],
        &format!("/v1/tasks/{next}/enable"),
        json!({}),
    )
    .await?;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["task"]["status"], "assigned");
    let cancel = ids["cancel"].as_str().unwrap();
    condition_workflow(&api, &tokens["pm"], cancel, "cancelled", None).await?;
    for key in ["no", "after_no"] {
        assert_eq!(
            api.server.get_task_api(ids[key].as_str().unwrap())?.status,
            "skipped"
        );
    }
    assert_eq!(api.server.get_task_api(first)?.output, Some(json!({})));
    Ok(())
}

#[tokio::test]
async fn conditional_parent_pause_resume_cancellation_and_enable_before_resolution() -> Result<()> {
    let (api, tokens) = task_workflow_api().await?;
    let parent = condition_parent(&api, &tokens["pm"]).await?;
    let (_, result) = batch_create(&api, &tokens["pm"], json!([
        {"key":"child", "channel":"#one", "title":"Wait", "assignee":"worker", "parent_task_id":parent, "optional":true,
            "condition":{"type":"parent_output_exists", "pointer":"/ok"}}
    ])).await?;
    let child = result["task_ids"]["child"].as_str().unwrap();
    let (_, body) = delegation_post(
        &api,
        &tokens["lead"],
        &format!("/v1/tasks/{child}/enable"),
        json!({}),
    )
    .await?;
    assert_eq!(body["task"]["status"], "dormant");
    assert_eq!(body["task"]["optional_enabled"], true);
    let lead = api.server.auth().resolve_by_name("lead")?.unwrap();
    let pm = api.server.auth().resolve_by_name("pm")?.unwrap();
    api.server.ask_task_api(&lead, &parent, "pause")?;
    assert_eq!(api.server.get_task_api(child)?.status, "blocked");
    let before = batch_task_state(&api)?;
    let (code, _) = delegation_post(
        &api,
        &tokens["pm"],
        &format!("/v1/tasks/{child}/enable"),
        json!({}),
    )
    .await?;
    assert_eq!(code, 400);
    assert_eq!(batch_task_state(&api)?, before);
    api.server.answer_task_api(&pm, &parent, "resume")?;
    assert_eq!(api.server.get_task_api(child)?.status, "dormant");
    condition_workflow(&api, &tokens["lead"], &parent, "testing", None).await?;
    condition_workflow(&api, &tokens["lead"], &parent, "review", None).await?;
    condition_workflow(
        &api,
        &tokens["pm"],
        &parent,
        "merged",
        Some(json!({"ok":true})),
    )
    .await?;
    assert_eq!(api.server.get_task_api(child)?.status, "assigned");
    let parent2 = condition_parent(&api, &tokens["pm"]).await?;
    let (_, result) = batch_create(&api, &tokens["pm"], json!([
        {"key":"child", "channel":"#one", "title":"Cancel me", "assignee":"worker", "parent_task_id":parent2, "optional":true}
    ])).await?;
    condition_workflow(&api, &tokens["pm"], &parent2, "cancelled", None).await?;
    assert_eq!(
        api.server
            .get_task_api(result["task_ids"]["child"].as_str().unwrap())?
            .status,
        "cancelled"
    );
    Ok(())
}

#[tokio::test]
async fn conditional_invalid_predicates_and_wait_cycles_roll_back_creation_and_dependencies(
) -> Result<()> {
    let (api, tokens) = task_workflow_api().await?;
    let parent = condition_parent(&api, &tokens["pm"]).await?;
    let other = condition_parent(&api, &tokens["pm"]).await?;
    let before = batch_task_state(&api)?;
    for predicate in [
        json!({"type":"execute", "script":"anything"}),
        json!({"type":"parent_output_exists", "pointer":"bad"}),
        json!({"type":"parent_output_exists", "pointer":"/bad~2"}),
        json!({"type":"parent_output_equals", "pointer":"/ok"}),
        json!({"type":"sibling_succeeded", "task_id":other}),
        json!({"type":"parent_output_exists", "pointer":"/ok", "extra":true}),
    ] {
        let (code, _) = batch_create(&api, &tokens["pm"], json!([
            {"channel":"#one", "title":"Valid"},
            {"channel":"#one", "title":"Invalid", "parent_task_id":parent, "condition":predicate}
        ])).await?;
        assert!((400..500).contains(&code));
        assert_eq!(batch_task_state(&api)?, before);
    }
    for extra in [
        json!({"optional":true}),
        json!({"condition":{"type":"parent_output_exists", "pointer":"/ok"}}),
    ] {
        let mut task = json!({"channel":"#one", "title":"No parent"});
        task.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        assert_eq!(
            batch_create(&api, &tokens["pm"], json!([task])).await?.0,
            400
        );
        assert_eq!(batch_task_state(&api)?, before);
    }
    let (code, _) = batch_create(&api, &tokens["pm"], json!([
        {"key":"a", "channel":"#one", "title":"A", "parent_task_id":parent, "condition":{"type":"sibling_succeeded", "task_id":"$b"}},
        {"key":"b", "channel":"#one", "title":"B", "parent_task_id":parent, "blocked_by":["$a"]}
    ])).await?;
    assert_eq!(code, 400);
    assert_eq!(batch_task_state(&api)?, before);
    let (code, result) = batch_create(&api, &tokens["pm"], json!([
        {"key":"child", "channel":"#one", "title":"Wait on parent", "parent_task_id":parent, "condition":{"type":"parent_output_exists", "pointer":"/ok"}},
        {"key":"nested", "channel":"#one", "title":"Inherited wait", "parent_task_id":"$child"}
    ])).await?;
    assert_eq!(code, 201, "{result}");
    let child = result["task_ids"]["child"].as_str().unwrap();
    let before = batch_task_state(&api)?;
    let (code, _) = api
        .request(
            "PUT",
            &format!("/v1/tasks/{parent}/dependencies"),
            Some(&tokens["pm"]),
            None,
            Some(&json!({"blocked_by":[child]})),
        )
        .await?;
    assert_eq!(code, 400);
    assert_eq!(batch_task_state(&api)?, before);
    let nested = result["task_ids"]["nested"].as_str().unwrap();
    let (code, _) = api
        .request(
            "PUT",
            &format!("/v1/tasks/{parent}/dependencies"),
            Some(&tokens["pm"]),
            None,
            Some(&json!({"blocked_by":[nested]})),
        )
        .await?;
    assert_eq!(code, 400);
    assert_eq!(batch_task_state(&api)?, before);
    // A gated branch may legitimately wait for its own child after activation.
    let (code, body) = api
        .request(
            "PUT",
            &format!("/v1/tasks/{child}/dependencies"),
            Some(&tokens["pm"]),
            None,
            Some(&json!({"blocked_by":[nested]})),
        )
        .await?;
    assert_eq!(code, 200, "{body}");
    Ok(())
}

#[tokio::test]
async fn conditional_output_validation_and_bulk_late_failure_leave_no_partial_decisions(
) -> Result<()> {
    let (api, tokens) = task_workflow_api().await?;
    let parent = condition_parent(&api, &tokens["pm"]).await?;
    let other = condition_parent(&api, &tokens["pm"]).await?;
    let (_, result) = batch_create(&api, &tokens["pm"], json!([
        {"key":"child", "channel":"#one", "title":"Branch", "parent_task_id":parent, "condition":{"type":"parent_output_exists", "pointer":"/ok"}}
    ])).await?;
    for id in [&parent, &other] {
        condition_workflow(&api, &tokens["lead"], id, "testing", None).await?;
        condition_workflow(&api, &tokens["lead"], id, "review", None).await?;
    }
    let before = batch_task_state(&api)?;
    for output in [
        json!(null),
        json!([]),
        json!("text"),
        json!(42),
        json!({"huge":"x".repeat(65536)}),
    ] {
        let (code, _) = delegation_post(
            &api,
            &tokens["pm"],
            &format!("/v1/tasks/{parent}/status"),
            json!({"status":"merged", "output":output}),
        )
        .await?;
        assert_eq!(code, 400);
        assert_eq!(batch_task_state(&api)?, before);
    }
    // The second task is not yet dependency-ready; the first completion and its branch must roll back.
    let child = result["task_ids"]["child"].as_str().unwrap();
    api.server
        .tasks()
        .set_dependencies(&other, &[child.into()], "pm")?;
    let before = batch_task_state(&api)?;
    let (code, _) = batch_status(
        &api,
        &tokens["pm"],
        json!([
            {"task_id":parent, "status":"merged", "output":{"ok":true}},
            {"task_id":other, "status":"merged", "output":{}}
        ]),
    )
    .await?;
    assert_eq!(code, 400);
    assert_eq!(batch_task_state(&api)?, before);
    Ok(())
}

#[tokio::test]
async fn conditional_enable_checks_token_scopes_and_does_not_grant_reviewer_or_peer_authority(
) -> Result<()> {
    let (api, tokens) = task_workflow_api().await?;
    let parent = condition_parent(&api, &tokens["pm"]).await?;
    let (_, result) = batch_create(&api, &tokens["pm"], json!([
        {"key":"child", "channel":"#one", "title":"Optional", "assignee":"worker", "reviewer":"peer", "parent_task_id":parent, "optional":true}
    ])).await?;
    let path = format!(
        "/v1/tasks/{}/enable",
        result["task_ids"]["child"].as_str().unwrap()
    );
    let before = batch_task_state(&api)?;
    for token in [None, Some("invalid")] {
        assert_eq!(
            api.request("POST", &path, token, None, Some(&json!({})))
                .await?
                .0,
            401
        );
    }
    for scopes in [vec!["read".into()], vec!["write".into()]] {
        let token =
            api.server
                .auth()
                .issue_token_with_options("pm", None, false, Some(&scopes), None)?;
        assert_eq!(
            delegation_post(&api, &token.token, &path, json!({}))
                .await?
                .0,
            403
        );
    }
    for nick in ["worker", "peer"] {
        assert_eq!(
            delegation_post(&api, &tokens[nick], &path, json!({}))
                .await?
                .0,
            400
        );
    }
    assert_eq!(batch_task_state(&api)?, before);
    Ok(())
}

#[tokio::test]
async fn conditional_leased_completion_records_output_and_requires_current_holder() -> Result<()> {
    let (api, tokens) = task_workflow_api().await?;
    let (code, body) = delegation_post(
        &api,
        &tokens["pm"],
        "/v1/tasks",
        json!({"channel":"#one", "title":"Leased parent"}),
    )
    .await?;
    assert_eq!(code, 201, "{body}");
    let parent = body["task"]["id"].as_str().unwrap();
    let (_, result) = batch_create(&api, &tokens["pm"], json!([
        {"key":"child", "channel":"#one", "title":"Lease branch", "parent_task_id":parent, "condition":{"type":"parent_output_equals", "pointer":"/ok", "value":true}}
    ])).await?;
    let path = format!("/v1/tasks/{parent}/done");
    assert_eq!(
        delegation_post(
            &api,
            &tokens["worker"],
            &format!("/v1/tasks/{parent}/claim"),
            json!({})
        )
        .await?
        .0,
        200
    );
    assert_eq!(
        delegation_post(&api, &tokens["peer"], &path, json!({"output":{"ok":true}}))
            .await?
            .0,
        409
    );
    let before = batch_task_state(&api)?;
    assert_eq!(
        delegation_post(&api, &tokens["worker"], &path, json!({"output":[]}))
            .await?
            .0,
        409
    );
    assert_eq!(batch_task_state(&api)?, before);
    let (code, body) = delegation_post(
        &api,
        &tokens["worker"],
        &path,
        json!({"output":{"ok":true}}),
    )
    .await?;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["task"]["output"], json!({"ok":true}));
    assert_eq!(
        api.server
            .get_task_api(result["task_ids"]["child"].as_str().unwrap())?
            .status,
        "open"
    );
    assert_eq!(
        delegation_post(&api, &tokens["worker"], &path, json!({"output":{}}))
            .await?
            .0,
        409
    );
    Ok(())
}
