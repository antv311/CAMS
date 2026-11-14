// Authenticated HTTP coverage of review authority, revision binding and routing.
const CONSENSUS_COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

async fn consensus_api(
    quorum: Option<u32>,
) -> Result<(TemplateApi, std::collections::HashMap<&'static str, String>)> {
    let (api, tokens) = task_workflow_api().await?;
    let mut group = delegation_group();
    group["review_policy"] =
        json!({"peer_reviewers": ["@LEAD", "peer", "worker"], "quorum": quorum});
    let (status, body) = api.write("POST", "/v1/groups", Some(&group)).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = api
        .write(
            "POST",
            "/v1/groups/team/attach",
            Some(&json!({"channel": "#one"})),
        )
        .await?;
    assert_eq!(status, StatusCode::OK);
    Ok((api, tokens))
}

async fn consensus_task(
    api: &TemplateApi,
    tokens: &std::collections::HashMap<&str, String>,
) -> Result<String> {
    let (status, body) = delegation_post(api, &tokens["pm"], "/v1/tasks",
        json!({"channel": "#one", "title": "Peer-reviewed work", "assignee": "worker", "reviewer": "lead"})).await?;
    assert_eq!(status, 201, "{body}");
    let id = body["task"]["id"].as_str().unwrap().to_string();
    let (status, body) = delegation_post(
        api,
        &tokens["worker"],
        &format!("/v1/tasks/{id}/status"),
        json!({"status": "testing"}),
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    Ok(id)
}

async fn consensus_submit(api: &TemplateApi, token: &str, id: &str) -> Result<Value> {
    let (status, body) = delegation_post(api, token, &format!("/v1/tasks/{id}/status"),
        json!({"status": "review", "commit": CONSENSUS_COMMIT, "branch": "feat/peer-test", "note": "cargo test: all passed"})).await?;
    assert_eq!(status, 200, "{body}");
    Ok(body["task"].clone())
}

async fn consensus_vote(
    api: &TemplateApi,
    token: &str,
    id: &str,
    revision: &str,
    decision: &str,
) -> Result<(u16, Value)> {
    delegation_post(api, token, &format!("/v1/tasks/{id}/peer-review"),
        json!({"revision": revision, "decision": decision, "note": "Inspected submitted commit and test evidence"})).await
}

#[tokio::test]
async fn consensus_round_binds_votes_routes_peers_and_resets_on_resubmission() -> Result<()> {
    let (api, tokens) = consensus_api(None).await?;
    let id = consensus_task(&api, &tokens).await?;
    let status_path = format!("/v1/tasks/{id}/status");
    let before = serde_json::to_value(api.server.get_task_api(&id)?)?;
    let events = api.server.tasks().events(&id)?.len();
    for payload in [
        json!({"status": "review", "note": "tests pass"}),
        json!({"status": "review", "commit": "main", "branch": "feat/x", "note": "tests pass"}),
        json!({"status": "review", "commit": CONSENSUS_COMMIT, "branch": "feat/x"}),
    ] {
        let (status, _) = delegation_post(&api, &tokens["worker"], &status_path, payload).await?;
        assert_eq!(status, 400);
    }
    assert_eq!(serde_json::to_value(api.server.get_task_api(&id)?)?, before);
    assert_eq!(api.server.tasks().events(&id)?.len(), events);
    let task = consensus_submit(&api, &tokens["worker"], &id).await?;
    let round = &task["review_round"];
    assert_eq!(round["quorum"], 2);
    assert_eq!(round["peer_reviewers"], json!(["lead", "peer"]));
    assert_eq!(round["commit"], CONSENSUS_COMMIT);
    assert_eq!(round["evidence"], "cargo test: all passed");
    let revision = round["revision"].as_str().unwrap();
    // Both peers, rather than just the single legacy reviewer, get review work.
    for nick in ["lead", "peer"] {
        let principal = api.server.auth().resolve_by_name(nick)?.unwrap();
        assert_eq!(api.server.inbox_api(&principal)?["review_count"], 1);
        let wakes = api
            .server
            .task_wake_recipients(&api.server.get_task_api(&id)?, "status")?;
        assert_eq!(
            wakes,
            ["lead".to_string(), "peer".to_string()]
                .into_iter()
                .collect()
        );
    }
    let before = serde_json::to_value(api.server.get_task_api(&id)?)?;
    let events = api.server.tasks().events(&id)?.len();
    let messages = api.server.history().tail("#one", 0, 100)?.len();
    for (nick, rev, decision) in [
        ("worker", revision, "approve"),
        ("pm", revision, "approve"),
        ("lead", "stale", "approve"),
        ("peer", revision, "invalid"),
    ] {
        assert_eq!(
            consensus_vote(&api, &tokens[nick], &id, rev, decision)
                .await?
                .0,
            400
        );
    }
    let unauthenticated = api
        .client
        .post(format!("{}/v1/tasks/{id}/peer-review", api.base))
        .json(&json!({"revision": revision, "decision": "approve", "note": "review"}))
        .send()
        .await?;
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(serde_json::to_value(api.server.get_task_api(&id)?)?, before);
    assert_eq!(api.server.tasks().events(&id)?.len(), events);
    assert_eq!(api.server.history().tail("#one", 0, 100)?.len(), messages);

    let (status, body) = consensus_vote(&api, &tokens["lead"], &id, revision, "approve").await?;
    assert_eq!(status, 200);
    assert_eq!(body["task"]["status"], "review");
    let lead = api.server.auth().resolve_by_name("lead")?.unwrap();
    assert_eq!(api.server.inbox_api(&lead)?["review_count"], 0);
    // Repeated approvals replace a vote and cannot manufacture quorum.
    let (_, body) = consensus_vote(&api, &tokens["lead"], &id, revision, "approve").await?;
    assert_eq!(
        body["task"]["review_round"]["votes"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(body["task"]["status"], "review");
    let (_, body) = consensus_vote(&api, &tokens["peer"], &id, revision, "reject").await?;
    assert_eq!(body["task"]["status"], "review");
    for to in ["super_review", "merged"] {
        assert_eq!(
            delegation_post(&api, &tokens["lead"], &status_path, json!({"status": to}))
                .await?
                .0,
            400
        );
    }
    let (_, body) = consensus_vote(&api, &tokens["peer"], &id, revision, "approve").await?;
    assert_eq!(body["task"]["status"], "super_review");
    let wakes = api
        .server
        .task_wake_recipients(&api.server.get_task_api(&id)?, "peer_review")?;
    assert!(wakes.contains("pm"));
    assert!(!wakes.contains("lead"));
    assert_eq!(
        delegation_post(
            &api,
            &tokens["lead"],
            &status_path,
            json!({"status": "merged"})
        )
        .await?
        .0,
        400
    );
    assert_eq!(
        delegation_post(
            &api,
            &tokens["worker"],
            &status_path,
            json!({"status": "in_progress", "branch": "changed"})
        )
        .await?
        .0,
        400
    );
    let (_, body) = delegation_post(
        &api,
        &tokens["lead"],
        &status_path,
        json!({"status": "changes_requested", "note": "fix coverage"}),
    )
    .await?;
    assert!(body["task"]["review_round"].is_null());
    let (status, _) = delegation_post(
        &api,
        &tokens["worker"],
        &status_path,
        json!({"status": "testing"}),
    )
    .await?;
    assert_eq!(status, 200);
    let task = consensus_submit(&api, &tokens["worker"], &id).await?;
    let new_revision = task["review_round"]["revision"].as_str().unwrap();
    assert_ne!(new_revision, revision);
    assert_eq!(task["review_round"]["votes"], json!([]));
    assert_eq!(
        consensus_vote(&api, &tokens["peer"], &id, revision, "approve")
            .await?
            .0,
        400
    );
    for nick in ["lead", "peer"] {
        assert_eq!(
            consensus_vote(&api, &tokens[nick], &id, new_revision, "approve")
                .await?
                .0,
            200
        );
    }
    let (status, body) = delegation_post(
        &api,
        &tokens["pm"],
        &status_path,
        json!({"status": "merged"}),
    )
    .await?;
    assert_eq!(status, 200);
    assert_eq!(body["task"]["status"], "merged");
    assert_eq!(
        consensus_vote(&api, &tokens["peer"], &id, new_revision, "reject")
            .await?
            .0,
        400
    );
    assert_eq!(
        api.server
            .tasks()
            .events(&id)?
            .iter()
            .filter(|e| e.action == "review_submission")
            .count(),
        2
    );
    Ok(())
}

#[tokio::test]
async fn consensus_quorum_rejection_and_audited_manager_owner_overrides() -> Result<()> {
    let (api, tokens) = consensus_api(Some(1)).await?;
    for manager in ["pm", "human"] {
        let id = consensus_task(&api, &tokens).await?;
        let task = consensus_submit(&api, &tokens["worker"], &id).await?;
        let revision = task["review_round"]["revision"].as_str().unwrap();
        let (_, body) = consensus_vote(&api, &tokens["lead"], &id, revision, "approve").await?;
        assert_eq!(body["task"]["status"], "super_review");
        // A later rejection blocks even when the approval count meets quorum.
        let (_, body) = consensus_vote(&api, &tokens["peer"], &id, revision, "reject").await?;
        assert_eq!(body["task"]["status"], "review");
        let principal = api.server.auth().resolve_by_name(manager)?.unwrap();
        assert!(api
            .server
            .set_task_status_api(&principal, &id, "merged", None, None, None, None)
            .is_err());
        let task = api.server.set_task_status_api(
            &principal,
            &id,
            "super_review",
            Some("Override reviewed with owner"),
            None,
            None,
            None,
        )?;
        assert_eq!(task.status, "super_review");
        let task = api.server.set_task_status_api(
            &principal,
            &id,
            "merged",
            Some("Accepted outstanding peer finding"),
            None,
            None,
            None,
        )?;
        assert_eq!(task.status, "merged");
        assert!(api
            .server
            .tasks()
            .events(&id)?
            .iter()
            .filter_map(|e| e.note.as_ref())
            .any(|note| note.starts_with("PM/owner consensus bypass:")));
    }
    Ok(())
}

#[tokio::test]
async fn consensus_reassignment_and_current_membership_invalidate_authority() -> Result<()> {
    let (api, tokens) = consensus_api(None).await?;
    let id = consensus_task(&api, &tokens).await?;
    let task = consensus_submit(&api, &tokens["worker"], &id).await?;
    let revision = task["review_round"]["revision"].as_str().unwrap();
    consensus_vote(&api, &tokens["lead"], &id, revision, "approve").await?;
    api.db.lock().unwrap().execute(
        "DELETE FROM project_roles WHERE channel = '#one' AND principal_id = 'lead'",
        [],
    )?;
    assert_eq!(
        consensus_vote(&api, &tokens["lead"], &id, revision, "approve")
            .await?
            .0,
        400
    );
    let (_, body) = consensus_vote(&api, &tokens["peer"], &id, revision, "approve").await?;
    assert_eq!(
        body["task"]["status"], "review",
        "departed peer's approval must not meet quorum"
    );
    let (status, body) = delegation_post(
        &api,
        &tokens["pm"],
        &format!("/v1/tasks/{id}/assign"),
        json!({"assignee": "peer"}),
    )
    .await?;
    assert_eq!(status, 200);
    assert_eq!(body["task"]["status"], "assigned");
    assert!(body["task"]["review_round"].is_null());
    assert_eq!(
        consensus_vote(&api, &tokens["peer"], &id, revision, "approve")
            .await?
            .0,
        400
    );
    Ok(())
}

#[tokio::test]
async fn consensus_group_policy_validation_and_membership_edits_are_atomic() -> Result<()> {
    let (api, _) = consensus_api(None).await?;
    let original = api.server.groups().get("team")?.unwrap();
    for policy in [
        json!({"peer_reviewers": []}),
        json!({"peer_reviewers": ["missing"]}),
        json!({"peer_reviewers": ["lead", "@LEAD"]}),
        json!({"peer_reviewers": ["peer"], "quorum": 0}),
        json!({"peer_reviewers": ["peer"], "quorum": 2}),
    ] {
        let mut group = delegation_group();
        group["review_policy"] = policy;
        let (status, _) = api.write("POST", "/v1/groups", Some(&group)).await?;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(api.server.groups().get("team")?.unwrap(), original);
    }
    let (status, _) = api
        .write(
            "PATCH",
            "/v1/groups/team",
            Some(&json!({
                "remove_workers": [{"template": "example", "model": "worker-model", "nick": "peer"}]
            })),
        )
        .await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(api.server.groups().get("team")?.unwrap(), original);
    // Full replacement can deliberately remove/replace the policy and roster together.
    let (status, _) = api
        .write("POST", "/v1/groups", Some(&delegation_group()))
        .await?;
    assert_eq!(status, StatusCode::OK);
    assert!(api
        .server
        .groups()
        .get("team")?
        .unwrap()
        .definition
        .review_policy
        .is_none());
    Ok(())
}

#[tokio::test]
async fn consensus_report_commit_sprint_scope_and_policy_resolution() -> Result<()> {
    let (api, tokens) = consensus_api(None).await?;
    let owner = api.server.auth().resolve_by_name("human")?.unwrap();
    let room = api
        .server
        .open_room_api(&owner, "#one", Some("Peer review"))?;
    let (status, body) = delegation_post(
        &api,
        &tokens["pm"],
        "/v1/tasks",
        json!({"channel": room.channel, "title": "Sprint work", "assignee": "worker"}),
    )
    .await?;
    assert_eq!(status, 201, "{body}");
    let id = body["task"]["id"].as_str().unwrap();
    let path = format!("/v1/tasks/{id}/status");
    delegation_post(&api, &tokens["worker"], &path, json!({"status": "testing"})).await?;
    let (status, body) = delegation_post(&api, &tokens["worker"], "/v1/reports",
        json!({"channel": room.channel, "title": "Validation", "status": "completed", "body": "Exact test output: PASS",
               "task_id": id, "git_commit": CONSENSUS_COMMIT})).await?;
    assert_eq!(status, 201, "{body}");
    let report = body["report"]["id"].as_str().unwrap();
    let request = json!({"status": "review", "branch": "feat/sprint", "report_id": report});
    let mut conflicting = request.clone();
    conflicting["commit"] = json!("f".repeat(40));
    assert_eq!(
        delegation_post(&api, &tokens["worker"], &path, conflicting)
            .await?
            .0,
        400
    );
    let (status, body) = delegation_post(&api, &tokens["worker"], &path, request.clone()).await?;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["task"]["channel"], "#one");
    assert_eq!(body["task"]["sprint"], room.channel);
    assert_eq!(body["task"]["review_round"]["commit"], CONSENSUS_COMMIT);
    assert!(body["task"]["review_round"]["evidence"]
        .as_str()
        .unwrap()
        .contains("Exact test output: PASS"));
    // Returning to work clears the round immediately, even before resubmission.
    let (_, body) = delegation_post(
        &api,
        &tokens["worker"],
        &path,
        json!({"status": "in_progress"}),
    )
    .await?;
    assert!(body["task"]["review_round"].is_null());
    delegation_post(&api, &tokens["worker"], &path, json!({"status": "testing"})).await?;
    // Self exclusion must not silently reduce an explicitly configured quorum.
    let mut group = delegation_group();
    group["review_policy"] = json!({"peer_reviewers": ["worker"], "quorum": 1});
    let (status, _) = api.write("POST", "/v1/groups", Some(&group)).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        delegation_post(&api, &tokens["worker"], &path, request.clone())
            .await?
            .0,
        400
    );
    group["review_policy"] = json!({"peer_reviewers": ["lead", "peer"]});
    api.write("POST", "/v1/groups", Some(&group)).await?;
    group["name"] = json!("second");
    api.write("POST", "/v1/groups", Some(&group)).await?;
    api.write(
        "POST",
        "/v1/groups/second/attach",
        Some(&json!({"channel": "#one"})),
    )
    .await?;
    let before = serde_json::to_value(api.server.get_task_api(id)?)?;
    let (status, body) = delegation_post(&api, &tokens["worker"], &path, request).await?;
    assert_eq!(status, 400);
    assert!(body["error"].as_str().unwrap().contains("ambiguous"));
    assert_eq!(serde_json::to_value(api.server.get_task_api(id)?)?, before);
    Ok(())
}
