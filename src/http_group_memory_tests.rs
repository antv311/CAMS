#[tokio::test]
async fn group_memory_http_checks_live_roster_and_never_accepts_owner_key_as_membership(
) -> Result<()> {
    let api = TemplateApi::start(Some("template-owner-key")).await?;
    let (_, worker) = api
        .server
        .auth()
        .register_principal("worker", Some("builder"), None)?;
    let (_, outsider) = api
        .server
        .auth()
        .register_principal("outsider", Some("owner"), None)?;
    let mut group = group_body();
    group["lead"]["nick"] = json!("@READER");
    group["workers"] = json!([
        {"template": "local", "model": "m", "nick": "worker"},
        {"template": "local", "model": "m", "count": 10}
    ]);
    api.write("POST", "/v1/groups", Some(&group)).await?;
    let path = "/v1/groups/dev/memory/notes";
    for token in [None, Some("invalid")] {
        assert_eq!(
            api.request("GET", path, token, None, None).await?.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            api.request(
                "POST",
                path,
                token,
                None,
                Some(&json!({"text": "bad", "append": true}))
            )
            .await?
            .0,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        api.write("POST", path, Some(&json!({"text": "bad", "append": true})))
            .await?
            .0,
        StatusCode::UNAUTHORIZED
    );
    let rejected = json!({"text": "bad", "append": true});
    for method in ["GET", "POST"] {
        assert_eq!(
            api.request(
                method,
                path,
                Some(&outsider),
                Some("template-owner-key"),
                if method == "POST" {
                    Some(&rejected)
                } else {
                    None
                }
            )
            .await?
            .0,
            StatusCode::FORBIDDEN
        );
    }
    let (_, empty) = api.read(path).await?;
    assert_eq!(empty["memory"]["revision"], 0);
    let (status, saved) = api
        .request(
            "POST",
            path,
            Some(&worker),
            None,
            Some(&json!({"text": "keep {task} é\n", "expected_revision": 0})),
        )
        .await?;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["memory"]["updated_by"], "worker");
    assert_eq!(api.read(path).await?.1, saved);
    // Removing a named seat revokes reads and writes immediately; no project role is needed.
    api.write(
        "PATCH",
        "/v1/groups/dev",
        Some(&json!({
            "remove_workers": [{"template": "local", "model": "m", "nick": "worker"}]
        })),
    )
    .await?;
    assert_eq!(
        api.request("GET", path, Some(&worker), None, None).await?.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        api.request(
            "POST",
            path,
            Some(&worker),
            None,
            Some(&json!({"text": "bad", "append": true}))
        )
        .await?
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(api.read(path).await?.1, saved);
    api.write(
        "PATCH",
        "/v1/groups/dev",
        Some(&json!({
            "add_workers": [{"template": "local", "model": "m", "nick": "outsider"}]
        })),
    )
    .await?;
    assert_eq!(
        api.request("GET", path, Some(&outsider), None, None)
            .await?
            .1,
        saved
    );
    Ok(())
}

#[tokio::test]
async fn group_memory_http_conflicts_validation_and_storage_errors() -> Result<()> {
    let api = TemplateApi::start(Some("template-owner-key")).await?;
    let mut group = group_body();
    group["name"] = json!("dev équipe");
    group["lead"]["nick"] = json!("reader");
    api.write("POST", "/v1/groups", Some(&group)).await?;
    let path = "/v1/groups/dev%20%C3%A9quipe/memory/scratchpad";
    let input = json!({"text": "initial", "expected_revision": 0});
    let (status, saved) = group_memory_post(&api, path, &input).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        group_memory_post(&api, path, &input).await?.0,
        StatusCode::CONFLICT
    );
    for invalid in [
        json!({"text": "no revision"}),
        json!({"text": " ", "append": true}),
        json!({"text": "nul\0", "expected_revision": 1}),
        json!({"text": "x".repeat(16_001), "expected_revision": 1}),
        json!({"text": "bad", "expected_revision": -1}),
    ] {
        assert_eq!(
            group_memory_post(&api, path, &invalid).await?.0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(api.read(path).await?.1, saved);
    }
    assert_eq!(
        api.read("/v1/groups/missing/memory/notes").await?.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        api.read("/v1/groups/dev%20%C3%A9quipe/memory/other")
            .await?
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        api.read("/v1/groups/dev%20%C3%A9quipe/memory/notes")
            .await?
            .1["memory"]["revision"],
        0
    );
    api.db.write()?.execute("DROP TABLE group_memory", [])?;
    assert_eq!(api.read(path).await?.0, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        group_memory_post(&api, path, &json!({"text": "bad", "append": true}))
            .await?
            .0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    Ok(())
}

async fn group_memory_post(
    api: &TemplateApi,
    path: &str,
    body: &Value,
) -> Result<(StatusCode, Value)> {
    api.request("POST", path, Some(&api.token), None, Some(body))
        .await
}
