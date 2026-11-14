// Authenticated HTTP coverage of task attachments: upload, list, download, limits and access.

async fn attach_raw(
    api: &TemplateApi,
    token: &str,
    path: &str,
    content_type: Option<&str>,
    body: Vec<u8>,
) -> Result<(u16, Value)> {
    let mut request = api
        .client
        .post(format!("{}{path}", api.base))
        .bearer_auth(token)
        .body(body);
    if let Some(content_type) = content_type {
        request = request.header(header::CONTENT_TYPE, content_type);
    }
    let response = request.send().await?;
    let status = response.status().as_u16();
    Ok((status, response.json().await.unwrap_or(Value::Null)))
}

#[tokio::test]
async fn task_attachments_upload_list_download_and_show_on_the_task() -> Result<()> {
    let (api, tokens) = task_workflow_api().await?;
    let (status, body) = delegation_post(
        &api,
        &tokens["pm"],
        "/v1/tasks",
        json!({"channel": "#one", "title": "Attach logs", "assignee": "worker"}),
    )
    .await?;
    assert_eq!(status, 201, "{body}");
    let id = body["task"]["id"].as_str().unwrap().to_string();
    let log = b"running 3 tests\ntest result: ok. 3 passed\n".to_vec();
    let (status, body) = attach_raw(
        &api,
        &tokens["worker"],
        &format!("/v1/tasks/{id}/attachments?name=logs%2Fcargo-test.log&note=cargo%20test"),
        None,
        log.clone(),
    )
    .await?;
    assert_eq!(status, 201, "{body}");
    let att = body["attachment"].clone();
    assert_eq!(att["filename"], "cargo-test.log");
    assert_eq!(att["kind"], "log");
    assert_eq!(att["size"], log.len());
    assert_eq!(att["uploaded_by"], "worker");
    assert_eq!(att["note"], "cargo test");
    let att_id = att["id"].as_str().unwrap().to_string();

    let (status, body) = attach_raw(
        &api,
        &tokens["pm"],
        &format!("/v1/tasks/{id}/attachments?name=shot.png&kind=evidence"),
        Some("image/png"),
        vec![0x89, b'P', b'N', b'G'],
    )
    .await?;
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["attachment"]["content_type"], "image/png");

    let (status, body) = call(
        &api.client,
        "GET",
        format!("{}/v1/tasks/{id}/attachments", api.base),
        &tokens["peer"],
        json!({}),
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    let listed = body["attachments"].as_array().unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0], att);
    assert!(listed[0].get("data").is_none());

    let (status, body) = call(
        &api.client,
        "GET",
        format!("{}/v1/tasks/{id}", api.base),
        &tokens["peer"],
        json!({}),
    )
    .await?;
    assert_eq!(status, 200);
    assert_eq!(body["attachments"].as_array().unwrap().len(), 2);
    assert!(body["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["action"] == "attach" && e["actor_id"] == "worker"));

    let response = api
        .client
        .get(format!("{}/v1/tasks/{id}/attachments/{att_id}", api.base))
        .bearer_auth(&tokens["peer"])
        .send()
        .await?;
    assert_eq!(response.status().as_u16(), 200);
    let headers = response.headers().clone();
    assert_eq!(headers[header::CONTENT_TYPE], "text/plain; charset=utf-8");
    assert_eq!(
        headers[header::CONTENT_DISPOSITION],
        "attachment; filename=\"cargo-test.log\""
    );
    assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    assert_eq!(headers["x-aircd-sha256"], att["sha256"].as_str().unwrap());
    assert_eq!(response.bytes().await?.to_vec(), log);

    let response = api
        .client
        .get(format!("{}/v1/tasks/{id}/attachments/{att_id}", api.base))
        .send()
        .await?;
    assert_eq!(response.status().as_u16(), 401);
    let (status, _) = call(
        &api.client,
        "GET",
        format!("{}/v1/tasks/{id}/attachments/att_missing", api.base),
        &tokens["peer"],
        json!({}),
    )
    .await?;
    assert_eq!(status, 404);
    Ok(())
}

#[tokio::test]
async fn task_attachments_reject_outsiders_bad_input_and_oversized_bodies() -> Result<()> {
    let (api, tokens) = task_workflow_api().await?;
    let (_, outsider) = api
        .server
        .auth()
        .register_principal("outsider", Some("builder"), None)?;
    let (status, body) = delegation_post(
        &api,
        &tokens["pm"],
        "/v1/tasks",
        json!({"channel": "#one", "title": "Attach logs", "assignee": "worker"}),
    )
    .await?;
    assert_eq!(status, 201, "{body}");
    let id = body["task"]["id"].as_str().unwrap().to_string();
    let path = format!("/v1/tasks/{id}/attachments?name=a.log");

    let (status, _) = attach_raw(&api, &outsider, &path, None, b"x".to_vec()).await?;
    assert_eq!(status, 403);
    let (status, _) = attach_raw(
        &api,
        &tokens["worker"],
        "/v1/tasks/task_missing/attachments?name=a.log",
        None,
        b"x".to_vec(),
    )
    .await?;
    assert_eq!(status, 404);
    let (status, _) = attach_raw(
        &api,
        &tokens["worker"],
        &format!("/v1/tasks/{id}/attachments"),
        None,
        b"x".to_vec(),
    )
    .await?;
    assert_eq!(status, 400);
    let (status, _) = attach_raw(&api, &tokens["worker"], &path, None, Vec::new()).await?;
    assert_eq!(status, 400);
    let (status, _) = attach_raw(
        &api,
        &tokens["worker"],
        &format!("{path}&kind=binary"),
        None,
        b"x".to_vec(),
    )
    .await?;
    assert_eq!(status, 400);
    let too_big = vec![b'x'; crate::attachments::MAX_ATTACHMENT_BYTES + 1];
    let (status, _) = attach_raw(&api, &tokens["worker"], &path, None, too_big).await?;
    assert_eq!(status, 400);
    let way_too_big = vec![b'x'; crate::attachments::MAX_ATTACHMENT_BYTES + 128 * 1024];
    let (status, _) = attach_raw(&api, &tokens["worker"], &path, None, way_too_big).await?;
    assert_eq!(status, 413);
    assert!(api.server.attachments().list(&id)?.is_empty());

    // A body that fits the attachment limit is accepted despite axum's smaller default.
    let large = vec![b'y'; 3 * 1024 * 1024];
    let (status, body) = attach_raw(&api, &tokens["worker"], &path, None, large).await?;
    assert_eq!(status, 201, "{body}");
    Ok(())
}
