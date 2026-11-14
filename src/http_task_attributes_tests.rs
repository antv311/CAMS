\\?\C:\Users\tony\Projects\CAMS\.aircd\worktrees\task-tags-metadata\src\http_task_attributes_tests.rs:

async fn attributes_request(
    api: &TemplateApi,
    token: &str,
    method: &str,
    path: &str,
    body: Value,
) -> Result<(u16, Value)> {
    let response = api
        .client
        .request(method.parse()?, format!("{}{path}", api.base))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await?;
    let code = response.status().as_u16();
    let text = response.text().await?;
    Ok((code, serde_json::from_str(&text).unwrap_or(json!(text))))
}

#[tokio::test]
async fn task_attributes_create_filter_and_batch_rollback() -> Result<()> {
    let (api, tokens) = task_workflow_api().await?;
    let meta = json!({"cost": 2.5, "nested":{"ok":true}, "list":[1,null,"x"], "null":null});
    let (code, body) = delegation_post(&api, &tokens["pm"], "/v1/tasks", json!({"channel":"#one", "title":"Tagged", "priority":"high", "tags":["release", "Release", "release", "a,b & +/%"], "metadata":meta})).await?;
    assert_eq!(code, 201, "{body}");
    let id = body["task"]["id"].as_str().unwrap();
    assert_eq!(
        body["task"]["tags"],
        json!(["Release", "a,b & +/%", "release"])
    );
    assert_eq!(body["task"]["metadata"], meta);
    for (tag, count) in [
        ("release", 1),
        ("Release", 1),
        ("RELEASE", 0),
        ("releas", 0),
        ("a,b & +/%", 1),
        ("' OR 1=1 --", 0),
    ] {
        let response = api
            .client
            .get(format!("{}/v1/tasks", api.base))
            .bearer_auth(&tokens["worker"])
            .query(&[
                ("channel", "#one"),
                ("tag", tag),
                ("priority", "high"),
                ("blocked", "false"),
            ])
            .send()
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        let result: Value = response.json().await?;
        assert_eq!(result["tasks"].as_array().unwrap().len(), count, "{tag}");
    }
    let (_, ordinary) = delegation_post(
        &api,
        &tokens["pm"],
        "/v1/tasks",
        json!({"channel":"#one","title":"Ordinary"}),
    )
    .await?;
    assert_eq!(ordinary["task"]["tags"], json!([]));
    assert_eq!(ordinary["task"]["metadata"], json!({}));
    let before = batch_task_state(&api)?;
    let (code, _) = batch_create(
        &api,
        &tokens["pm"],
        json!([
            {"channel":"#one", "title":"Valid", "tags":["batch"], "metadata":{"index":1}},
            {"channel":"#one", "title":"Invalid", "tags":[""]}
        ]),
    )
    .await?;
    assert_eq!(code, 400);
    assert_eq!(batch_task_state(&api)?, before);
    let (code, batch) = batch_create(
        &api,
        &tokens["pm"],
        json!([
            {"channel":"#one", "title":"Valid", "tags":["batch"], "metadata":{"index":1}}
        ]),
    )
    .await?;
    assert_eq!(code, 201, "{batch}");
    assert_eq!(batch["tasks"][0]["metadata"], json!({"index":1}));
    let (code, shown) = attributes_request(
        &api,
        &tokens["worker"],
        "GET",
        &format!("/v1/tasks/{id}"),
        Value::Null,
    )
    .await?;
    assert_eq!(code, 200);
    assert_eq!(shown["task"]["metadata"], meta);
    Ok(())
}

#[tokio::test]
async fn task_attributes_permission_audit_and_workflow_isolation() -> Result<()> {
    let (api, tokens) = task_workflow_api().await?;
    let parent = condition_parent(&api, &tokens["pm"]).await?;
    let (_, body) = delegation_post(&api, &tokens["lead"], "/v1/tasks", json!({"channel":"#one", "title":"Child", "parent_task_id":parent,"assignee":"worker", "optional":true})).await?;
    let id = body["task"]["id"].as_str().unwrap();
    let path = format!("/v1/tasks/{id}/attributes");
    let readonly = api.server.auth().issue_token_with_options(
        "worker",
        Some("read attributes"),
        false,
        Some(&["read".into()]),
        None,
    )?;
    let write = api.server.auth().issue_token_with_options(
        "worker",
        Some("write attributes"),
        false,
        Some(&["write".into()]),
        None,
    )?;
    assert_eq!(
        attributes_request(
            &api,
            &readonly.token,
            "PATCH",
            &path,
            json!({"tags":["denied"]})
        )
        .await?
        .0,
        403
    );
    assert_eq!(
        attributes_request(
            &api,
            &write.token,
            "PATCH",
            &path,
            json!({"metadata":{"write_scope":true}})
        )
        .await?
        .0,
        200
    );
    let before = api.server.get_task_api(id)?;
    let count = api.server.tasks().events(id)?.len();
    let (code, _) = attributes_request(
        &api,
        &tokens["peer"],
        "PATCH",
        &path,
        json!({"tags":["denied"]}),
    )
    .await?;
    assert_eq!(code, 400);
    assert_eq!(api.server.get_task_api(id)?, before);
    assert_eq!(api.server.tasks().events(id)?.len(), count);
    for nick in ["worker", "lead", "pm"] {
        let (code, body) = attributes_request(
            &api,
            &tokens[nick],
            "PATCH",
            &path,
            json!({"tags":[nick],"metadata":{"edited_by":nick}}),
        )
        .await?;
        assert_eq!(code, 200, "{body}");
        assert_eq!(body["task"]["status"], "dormant");
        assert_eq!(body["task"]["output"], Value::Null);
    }
    let count = api.server.tasks().events(id)?.len();
    attributes_request(
        &api,
        &tokens["pm"],
        "PATCH",
        &path,
        json!({"tags":["pm"],"metadata":{"edited_by":"pm"}}),
    )
    .await?;
    assert_eq!(
        api.server.tasks().events(id)?.len(),
        count,
        "no-op must not add an event"
    );
    let (_, body) =
        attributes_request(&api, &tokens["worker"], "PATCH", &path, json!({"tags":[]})).await?;
    assert_eq!(body["task"]["metadata"], json!({"edited_by":"pm"}));
    let (_, body) = attributes_request(
        &api,
        &tokens["worker"],
        "PATCH",
        &path,
        json!({"metadata":{}}),
    )
    .await?;
    assert_eq!(body["task"]["tags"], json!([]));
    assert_eq!(body["task"]["metadata"], json!({}));
    let events = api.server.tasks().events(id)?;
    assert_eq!(events.last().unwrap().action, "attributes");
    assert_eq!(
        events.last().unwrap().from_status.as_deref(),
        Some("dormant")
    );
    assert!(!events
        .last()
        .unwrap()
        .note
        .as_deref()
        .unwrap()
        .contains("edited_by"));
    // Completed tasks remain annotatable without modifying immutable output.
    let (_, leased) = delegation_post(
        &api,
        &tokens["pm"],
        "/v1/tasks",
        json!({"channel":"#one","title":"Lease"}),
    )
    .await?;
    let lease_id = leased["task"]["id"].as_str().unwrap();
    delegation_post(
        &api,
        &tokens["worker"],
        &format!("/v1/tasks/{lease_id}/claim"),
        json!({}),
    )
    .await?;
    let (code, _) = attributes_request(
        &api,
        &tokens["worker"],
        "PATCH",
        &format!("/v1/tasks/{lease_id}/attributes"),
        json!({"tags":["leased"]}),
    )
    .await?;
    assert_eq!(code, 200);
    delegation_post(
        &api,
        &tokens["worker"],
        &format!("/v1/tasks/{lease_id}/done"),
        json!({"output":{"result":"ok"}}),
    )
    .await?;
    let (code, _) = attributes_request(
        &api,
        &tokens["worker"],
        "PATCH",
        &format!("/v1/tasks/{lease_id}/attributes"),
        json!({"tags":["stale-holder"]}),
    )
    .await?;
    assert_eq!(code, 400);
    let (code, body) = attributes_request(
        &api,
        &tokens["pm"],
        "PATCH",
        &format!("/v1/tasks/{lease_id}/attributes"),
        json!({"metadata":{"archived":true}}),
    )
    .await?;
    assert_eq!(code, 200);
    assert_eq!(body["task"]["status"], "done");
    assert_eq!(body["task"]["output"], json!({"result":"ok"}));
    Ok(())
}

#[tokio::test]
async fn task_attributes_invalid_fields_are_atomic() -> Result<()> {
    let (api, tokens) = task_workflow_api().await?;
    let id = condition_parent(&api, &tokens["pm"]).await?;
    let path = format!("/v1/tasks/{id}/attributes");
    let before = api.server.get_task_api(&id)?;
    let events = api.server.tasks().events(&id)?.len();
    for invalid in [
        json!({}),
        json!({"tags":null}),
        json!({"tags":[1]}),
        json!({"tags":[""]}),
        json!({"tags":[" \t"]}),
        json!({"tags":["a\nb"]}),
        json!({"tags":["x".repeat(129)]}),
        json!({"tags":vec!["x";65]}),
        json!({"metadata":null}),
        json!({"metadata":[]}),
        json!({"metadata":"bad"}),
        json!({"metadata":{"x":"x".repeat(65536)}}),
        json!({"tags":["valid"],"status":"merged"}),
    ] {
        let (code, _) =
            attributes_request(&api, &tokens["pm"], "PATCH", &path, invalid.clone()).await?;
        assert!((400..500).contains(&code), "{invalid}: {code}");
        assert_eq!(api.server.get_task_api(&id)?, before);
        assert_eq!(api.server.tasks().events(&id)?.len(), events);
    }
    Ok(())
}
