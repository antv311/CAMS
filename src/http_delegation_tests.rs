// HTTP and real child-process coverage for the group lead workflow.
fn delegation_group() -> Value {
    json!({
        "name": "team",
        "lead": {"template": "example", "model": "lead-model", "nick": "@Lead"},
        "workers": [
            {"template": "example", "model": "worker-model", "nick": "Worker"},
            {"template": "example", "model": "worker-model", "nick": "peer"},
            {"template": "example", "model": "worker-model", "count": 3}
        ],
        "lead_prompt": "LEAD {group} {task_id} {task_title} {task_body}",
        "worker_prompt": "WORKER {group} {parent_task_id} {reviewer} {task_body}"
    })
}

async fn attach_delegation_group(api: &TemplateApi) -> Result<()> {
    let (status, body) = api
        .write("POST", "/v1/groups", Some(&delegation_group()))
        .await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = api
        .write(
            "POST",
            "/v1/groups/team/attach",
            Some(&json!({"channel": "#one"})),
        )
        .await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    Ok(())
}

async fn delegation_post(
    api: &TemplateApi,
    token: &str,
    path: &str,
    body: Value,
) -> Result<(u16, Value)> {
    call(
        &api.client,
        "POST",
        format!("{}{path}", api.base),
        token,
        body,
    )
    .await
}

#[tokio::test]
async fn group_delegation_enforces_current_named_roster_without_mutating_rejections() -> Result<()>
{
    let (api, tokens) = task_workflow_api().await?;
    attach_delegation_group(&api).await?;
    let owner = api.server.auth().resolve_by_name("human")?.unwrap();
    let (_, outsider_token) =
        api.server
            .auth()
            .register_principal("outsider", Some("builder"), None)?;
    api.server
        .set_project_member_api(&owner, "#one", "outsider", Some("builder"))?;
    let (_, body) = delegation_post(
        &api,
        &tokens["pm"],
        "/v1/tasks",
        json!({
            "channel": "#one", "title": "Parent", "assignee": "lead"
        }),
    )
    .await?;
    let parent = body["task"]["id"].as_str().unwrap();
    let input = json!({"channel": "#one", "title": "Child", "assignee": "@WORKER", "parent_task_id": parent});
    let (status, body) = delegation_post(&api, &tokens["lead"], "/v1/tasks", input.clone()).await?;
    assert_eq!(status, 201, "{body}");
    let child = body["task"]["id"].as_str().unwrap();
    let before = serde_json::to_value(api.server.get_task_api(child)?)?;
    let events_before = api.server.tasks().events(child)?.len();
    let count_before = api.server.tasks().list("#one")?.len();
    let history_before = api.server.history().tail("#one", 0, 100)?.len();
    for target in ["outsider", "reader", "missing"] {
        let mut invalid = input.clone();
        invalid["assignee"] = json!(target);
        let (status, body) = delegation_post(&api, &tokens["lead"], "/v1/tasks", invalid).await?;
        assert_eq!(status, 400, "{body}");
        let (status, body) = delegation_post(
            &api,
            &tokens["lead"],
            &format!("/v1/tasks/{child}/assign"),
            json!({"assignee": target}),
        )
        .await?;
        assert_eq!(status, 400, "{body}");
    }
    // Self-assignment cannot bypass the parent's group roster.
    let (status, _) = delegation_post(
        &api,
        &outsider_token,
        &format!("/v1/tasks/{child}/assign"),
        json!({"assignee": "outsider"}),
    )
    .await?;
    assert_eq!(status, 400);
    assert_eq!(
        serde_json::to_value(api.server.get_task_api(child)?)?,
        before
    );
    assert_eq!(api.server.tasks().events(child)?.len(), events_before);
    assert_eq!(api.server.tasks().list("#one")?.len(), count_before);
    assert_eq!(
        api.server.history().tail("#one", 0, 100)?.len(),
        history_before
    );

    // Membership changes apply to the very next delegation, without provisioning.
    let (status, body) = api.write("PATCH", "/v1/groups/team", Some(&json!({
        "remove_workers": [{"template": "example", "model": "worker-model", "nick": "Worker"}],
        "add_workers": [{"template": "example", "model": "worker-model", "nick": "outsider"}]
    }))).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = delegation_post(&api, &tokens["lead"], "/v1/tasks", input.clone()).await?;
    assert_eq!(status, 400);
    let (status, _) = delegation_post(
        &api,
        &tokens["lead"],
        &format!("/v1/tasks/{child}/assign"),
        json!({"assignee": "outsider"}),
    )
    .await?;
    assert_eq!(status, 200);
    // PM assignment remains project-wide; no group-wide project role is granted.
    let (status, _) = delegation_post(
        &api,
        &tokens["pm"],
        &format!("/v1/tasks/{child}/assign"),
        json!({"assignee": "worker"}),
    )
    .await?;
    assert_eq!(status, 200);
    let (status, _) = delegation_post(
        &api,
        &tokens["lead"],
        "/v1/tasks",
        json!({"channel": "#one", "title": "Unrelated", "assignee": "peer"}),
    )
    .await?;
    assert_eq!(status, 400);
    // Multiple bindings fail closed, as wake prompt selection does.
    let mut ambiguous = delegation_group();
    ambiguous["name"] = json!("second");
    api.write("POST", "/v1/groups", Some(&ambiguous)).await?;
    api.write(
        "POST",
        "/v1/groups/second/attach",
        Some(&json!({"channel": "#one"})),
    )
    .await?;
    let (status, body) = delegation_post(&api, &tokens["lead"], "/v1/tasks", input).await?;
    assert_eq!(status, 400);
    assert!(body["error"]
        .as_str()
        .unwrap()
        .contains("ambiguous group role"));
    Ok(())
}

#[tokio::test]
async fn group_delegation_inherits_parent_context_and_lists_all_children() -> Result<()> {
    let (api, tokens) = task_workflow_api().await?;
    attach_delegation_group(&api).await?;
    let owner = api.server.auth().resolve_by_name("human")?.unwrap();
    api.server.open_room_api(&owner, "#one", None)?;
    api.server
        .gits()
        .upsert("repo-one", "#one", "example/repo", None, None, None)?;
    let (_, body) = delegation_post(
        &api,
        &tokens["pm"],
        "/v1/tasks",
        json!({
            "channel": "#one-s1", "title": "Parent", "assignee": "lead", "repo": "repo-one"
        }),
    )
    .await?;
    let parent = body["task"]["id"].as_str().unwrap();
    api.server.open_room_api(&owner, "#one", None)?;
    let input = json!({"channel": "#one", "title": "Inherited", "assignee": "worker", "parent_task_id": parent});
    let (status, body) = delegation_post(&api, &tokens["lead"], "/v1/tasks", input.clone()).await?;
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["task"]["sprint"], "#one-s1");
    assert_eq!(body["task"]["repo"], "repo-one");
    assert_eq!(body["task"]["reviewer"], "lead");
    let first = body["task"]["id"].as_str().unwrap().to_string();
    let mut explicit = input;
    explicit["channel"] = json!("#one-s2");
    let (_, body) = delegation_post(&api, &tokens["lead"], "/v1/tasks", explicit).await?;
    assert_eq!(body["task"]["sprint"], "#one-s2");
    api.server
        .set_task_status_api(&owner, &first, "cancelled", None, None, None, None)?;
    let (_, body) = call(
        &api.client,
        "GET",
        format!("{}/v1/tasks/{parent}", api.base),
        &tokens["lead"],
        Value::Null,
    )
    .await?;
    let children = body["subtasks"].as_array().unwrap();
    assert_eq!(children.len(), 2);
    assert_eq!(children[0]["id"], first);
    assert_eq!(children[0]["status"], "cancelled");
    assert_eq!(children[1]["sprint"], "#one-s2");
    Ok(())
}

#[test]
fn delegation_probe_child() {
    if std::env::var("AIRCD_DELEGATION_PROBE").as_deref() == Ok("1") {
        println!(
            "DELEGATION {}",
            json!({
                "nick": std::env::var("AIRC_NICK").unwrap(),
                "task": std::env::var("AIRCD_WAKE_TASK").unwrap(),
                "channel": std::env::var("AIRCD_WAKE_CHANNEL").unwrap(),
                "repo": std::env::var("AIRCD_WAKE_REPO").unwrap(),
                "prompt": std::env::var("DELEGATION_PROMPT").unwrap()
            })
        );
    }
}

async fn expect_delegation_wake(
    api: &TemplateApi,
    total: usize,
    nick: &str,
    task: &str,
) -> Result<()> {
    expect_delegation_wakes(api, total, &[nick], task).await
}

async fn expect_delegation_wakes(
    api: &TemplateApi,
    total: usize,
    nicks: &[&str],
    task: &str,
) -> Result<()> {
    for _ in 0..250 {
        let runs = api.server.wakes().runs(None, 200)?;
        if runs.len() >= total && runs.iter().all(|run| run.status == "ok") {
            assert_eq!(runs.len(), total, "unexpected recipients: {runs:?}");
            let actual: std::collections::HashSet<_> = runs[..nicks.len()]
                .iter()
                .map(|r| r.principal_id.as_str())
                .collect();
            assert_eq!(actual, nicks.iter().copied().collect());
            for latest in &runs[..nicks.len()] {
                assert_eq!(latest.task_id.as_deref(), Some(task));
                assert_eq!(latest.channel, "#one-s1");
                assert!(latest.log_tail.as_deref().unwrap().contains("DELEGATION "));
            }
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    anyhow::bail!(
        "wake did not complete: {:?}",
        api.server.wakes().runs(None, 200)?
    )
}

#[tokio::test]
async fn group_delegation_routes_real_wakes_questions_reviews_and_escalation() -> Result<()> {
    let (api, tokens) = task_workflow_api().await?;
    attach_delegation_group(&api).await?;
    let owner = api.server.auth().resolve_by_name("human")?.unwrap();
    api.server.open_room_api(&owner, "#one", None)?;
    let repo = std::env::current_dir()?.to_string_lossy().into_owned();
    api.server
        .gits()
        .upsert("repo-one", "#one", &repo, None, None, None)?;
    for nick in ["human", "pm", "lead", "worker", "peer"] {
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
    let (_, body) = delegation_post(
        &api,
        &tokens["pm"],
        "/v1/tasks",
        json!({
            "channel": "#one", "title": "Parent @peer @here", "assignee": "lead", "repo": "repo-one"
        }),
    )
    .await?;
    let parent = body["task"]["id"].as_str().unwrap();
    expect_delegation_wake(&api, 1, "lead", parent).await?;
    let (_, body) = delegation_post(
        &api,
        &tokens["lead"],
        "/v1/tasks",
        json!({
            "channel": "#one", "title": "Child @pm @peer", "body": "Only child context {nick}",
            "assignee": "worker", "parent_task_id": parent
        }),
    )
    .await?;
    let child = body["task"]["id"].as_str().unwrap();
    expect_delegation_wake(&api, 2, "worker", child).await?;
    let run = api.server.wakes().runs(Some("worker"), 1)?.remove(0);
    let log = run.log_tail.unwrap();
    assert!(log.contains(&repo.replace('\\', "\\\\")), "{log}");
    assert!(
        log.contains(&format!(
            "WORKER team {parent} lead Only child context {{nick}}"
        )),
        "{log}"
    );

    let child_path = |action| format!("/v1/tasks/{child}/{action}");
    let (status, _) = delegation_post(
        &api,
        &tokens["worker"],
        &child_path("ask"),
        json!({"question": "API shape? @pm @peer @here"}),
    )
    .await?;
    assert_eq!(status, 200);
    expect_delegation_wake(&api, 3, "lead", child).await?;
    for nick in ["lead", "pm"] {
        let (_, inbox) = call(
            &api.client,
            "GET",
            format!("{}/v1/inbox", api.base),
            &tokens[nick],
            Value::Null,
        )
        .await?;
        assert_eq!(inbox["inbox"]["question_count"], 1);
    }
    let (status, _) = delegation_post(
        &api,
        &tokens["peer"],
        &child_path("answer"),
        json!({"answer": "unauthorized"}),
    )
    .await?;
    assert_eq!(status, 400);
    let (status, body) = delegation_post(
        &api,
        &tokens["lead"],
        &child_path("answer"),
        json!({"answer": "Use PATCH"}),
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["task"]["status"], "assigned");
    expect_delegation_wake(&api, 4, "worker", child).await?;

    delegation_post(
        &api,
        &tokens["worker"],
        &child_path("ask"),
        json!({"question": "Scope decision?"}),
    )
    .await?;
    expect_delegation_wake(&api, 5, "lead", child).await?;
    let (status, _) = delegation_post(
        &api,
        &tokens["lead"],
        &child_path("escalate"),
        json!({"note": "Needs PM scope"}),
    )
    .await?;
    assert_eq!(status, 200);
    expect_delegation_wake(&api, 6, "pm", child).await?;
    assert_eq!(
        api.server.tasks().pending_question(child)?.unwrap().status,
        "escalated_pm"
    );
    let lead = api.server.auth().resolve_by_name("lead")?.unwrap();
    assert_eq!(api.server.inbox_api(&lead)?["question_count"], 0);
    let pm = api.server.auth().resolve_by_name("pm")?.unwrap();
    assert_eq!(api.server.inbox_api(&pm)?["question_count"], 1);
    let (status, _) = delegation_post(
        &api,
        &tokens["lead"],
        &child_path("answer"),
        json!({"answer": "Cannot take back escalation"}),
    )
    .await?;
    assert_eq!(status, 400);
    let (status, _) = delegation_post(
        &api,
        &tokens["pm"],
        &child_path("escalate"),
        json!({"note": "Owner decision"}),
    )
    .await?;
    assert_eq!(status, 200);
    expect_delegation_wake(&api, 7, "human", child).await?;
    assert_eq!(
        api.server.inbox_api(&owner)?["needs_you"][0]["task_id"],
        child
    );
    let (status, _) = delegation_post(
        &api,
        "human-token",
        &child_path("answer"),
        json!({"answer": "Approved scope"}),
    )
    .await?;
    assert_eq!(status, 200);
    expect_delegation_wake(&api, 8, "worker", child).await?;
    delegation_post(
        &api,
        &tokens["worker"],
        &child_path("status"),
        json!({"status": "testing"}),
    )
    .await?;
    let (status, _) = delegation_post(
        &api,
        &tokens["worker"],
        &child_path("status"),
        json!({"status": "review", "note": "cargo test: PASS @pm @peer @here"}),
    )
    .await?;
    assert_eq!(status, 200);
    expect_delegation_wake(&api, 9, "lead", child).await?;
    assert_eq!(
        api.server.inbox_api(&pm)?["review_count"],
        1,
        "PM retains review oversight"
    );
    let (status, _) = delegation_post(
        &api,
        &tokens["lead"],
        &child_path("status"),
        json!({"status": "changes_requested", "note": "Add case"}),
    )
    .await?;
    assert_eq!(status, 200);
    expect_delegation_wake(&api, 10, "worker", child).await?;
    delegation_post(
        &api,
        &tokens["worker"],
        &child_path("status"),
        json!({"status": "testing"}),
    )
    .await?;
    delegation_post(
        &api,
        &tokens["worker"],
        &child_path("status"),
        json!({"status": "review", "note": "cargo test: PASS"}),
    )
    .await?;
    expect_delegation_wake(&api, 11, "lead", child).await?;
    delegation_post(
        &api,
        &tokens["lead"],
        &child_path("status"),
        json!({"status": "merged"}),
    )
    .await?;
    expect_delegation_wake(&api, 12, "worker", child).await?;
    delegation_post(
        &api,
        &tokens["lead"],
        &format!("/v1/tasks/{parent}/status"),
        json!({"status": "testing"}),
    )
    .await?;
    delegation_post(
        &api,
        &tokens["lead"],
        &format!("/v1/tasks/{parent}/status"),
        json!({"status": "review", "note": "Combined result PASS"}),
    )
    .await?;
    expect_delegation_wakes(&api, 14, &["human", "pm"], parent).await?;
    Ok(())
}
