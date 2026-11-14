use super::*;

fn executing(store: &TaskStore, timeout: Option<u32>) -> Result<Task> {
    let task = store.create_full(NewTask {
        channel: "#one".into(),
        title: "Work".into(),
        assignee: Some("worker".into()),
        branch: Some("feat/preserve".into()),
        timeout_seconds: timeout,
        ..Default::default()
    })?;
    store.set_status(&task.id, "assigned", "in_progress", "worker", None, None)
}

#[test]
fn timeout_boundary_recovers_once_preserves_owner_branch_and_audits() -> Result<()> {
    let db = crate::store::open(":memory:")?;
    let store = TaskStore::new(db.clone(), 60);
    let task = executing(&store, Some(30))?;
    let at = task.execution_heartbeat_at.unwrap();
    assert!(store.recover_timeouts(at + 29)?.is_empty());
    let recovered = store.recover_timeouts(at + 30)?;
    assert_eq!(recovered.len(), 1);
    let task = &recovered[0].task;
    assert_eq!(task.status, "assigned");
    assert_eq!(task.assignee.as_deref(), Some("worker"));
    assert_eq!(task.branch.as_deref(), Some("feat/preserve"));
    assert!(task.execution_heartbeat_at.is_none());
    assert!(task.claimed_by.is_none());
    assert!(store.recover_timeouts(at + 300)?.is_empty());
    let events = store.events(&task.id)?;
    let event = events.last().unwrap();
    assert_eq!(event.action, "timeout");
    assert_eq!(event.from_status.as_deref(), Some("in_progress"));
    assert_eq!(event.to_status.as_deref(), Some("assigned"));
    assert!(event
        .note
        .as_deref()
        .unwrap()
        .contains("previous_holder=worker"));
    let metrics = crate::task_metrics::compute(task, &events, at + 40);
    assert_eq!(metrics.time_in_status["in_progress"], 30);
    Ok(())
}

#[test]
fn timeout_policy_precedence_default_disabled_and_validation() -> Result<()> {
    let db = crate::store::open(":memory:")?;
    let disabled = TaskStore::new(db.clone(), 60);
    let unlimited = executing(&disabled, None)?;
    assert!(disabled
        .recover_timeouts(unlimited.updated_at + 100000)?
        .is_empty());
    let config: TaskTimeoutConfig =
        toml::from_str("default_seconds = 100\n[priority_seconds]\nhigh = 20\n")?;
    let store = disabled.with_timeout_config(config)?;
    let task = executing(&store, Some(40))?;
    store.set_priority(&task.id, "high", "pm")?;
    let task = store.get(&task.id)?.unwrap();
    let at = task.execution_heartbeat_at.unwrap();
    assert!(store.recover_timeouts(at + 39)?.is_empty());
    assert_eq!(store.recover_timeouts(at + 40)?.len(), 1);
    let priority = executing(&store, None)?;
    store.set_priority(&priority.id, "high", "pm")?;
    assert_eq!(store.recover_timeouts(priority.updated_at + 20)?.len(), 1);
    assert_eq!(store.recover_timeouts(unlimited.updated_at + 100)?.len(), 1);
    for raw in [
        "default_seconds = 0",
        "default_seconds = -1",
        "default_seconds = 1.5",
        "[priority_seconds]\nunknown = 30",
        "[priority_seconds]\nlow = 0",
    ] {
        assert!(toml::from_str::<TaskTimeoutConfig>(raw)
            .map_err(anyhow::Error::from)
            .and_then(|c| c.validate())
            .is_err());
    }
    assert!(store.set_timeout(&task.id, Some(0), "pm").is_err());
    Ok(())
}

#[test]
fn expired_claim_reopens_and_late_completion_or_renewal_cannot_win() -> Result<()> {
    let db = crate::store::open(":memory:")?;
    let store = TaskStore::new(db.clone(), 60);
    let task = store.create_full(NewTask {
        channel: "#one".into(),
        title: "Claim".into(),
        timeout_seconds: Some(10),
        ..Default::default()
    })?;
    let claimed = store.claim(&task.id, "old")?;
    assert_eq!(claimed.lease_expires_at, Some(claimed.updated_at + 10));
    db.write()?.execute(
        "UPDATE tasks SET lease_expires_at = ?2 WHERE id = ?1",
        params![task.id, unix_timestamp()],
    )?;
    assert!(store.heartbeat(&task.id, "old").is_err());
    assert!(store.done(&task.id, "old").is_err());
    assert_eq!(store.recover_timeouts(unix_timestamp())?.len(), 1);
    let reopened = store.get(&task.id)?.unwrap();
    assert_eq!(reopened.status, "open");
    assert!(reopened.claimed_by.is_none() && reopened.lease_expires_at.is_none());
    store.claim(&task.id, "new")?;
    assert!(store.done(&task.id, "old").is_err());
    assert!(store.heartbeat(&task.id, "old").is_err());
    assert_eq!(store.done(&task.id, "new")?.status, "done");
    Ok(())
}

#[test]
fn heartbeat_renews_only_its_live_task_and_metadata_does_not_mask_stalls() -> Result<()> {
    let db = crate::store::open(":memory:")?;
    let store = TaskStore::new(db.clone(), 60);
    let task = executing(&store, Some(60))?;
    let now = unix_timestamp();
    db.write()?.execute(
        "UPDATE tasks SET execution_heartbeat_at = ?2 WHERE id = ?1",
        params![task.id, now - 50],
    )?;
    assert!(store.heartbeat(&task.id, "other").is_err());
    assert_eq!(
        store.heartbeat(&task.id, "worker")?.execution_heartbeat_at,
        Some(now)
    );
    assert!(store.recover_timeouts(now + 59)?.is_empty());
    store.set_priority(&task.id, "urgent", "pm")?;
    assert_eq!(store.recover_timeouts(now + 60)?.len(), 1);
    assert!(store.heartbeat(&task.id, "worker").is_err());
    let claim = store.create("#one", "Live claim")?;
    store.claim(&claim.id, "worker")?;
    db.write()?.execute(
        "UPDATE tasks SET lease_expires_at = ?2 WHERE id = ?1",
        params![claim.id, now + 5],
    )?;
    let renewed = store.heartbeat(&claim.id, "worker")?;
    assert!(renewed.lease_expires_at.unwrap() >= now + 60);
    assert!(store.recover_timeouts(now + 5)?.is_empty());
    Ok(())
}

#[test]
fn timeout_exempts_waiting_terminal_and_parent_paused_work() -> Result<()> {
    let db = crate::store::open(":memory:")?;
    let store = TaskStore::new(db.clone(), 60);
    for status in ALL_STATUSES
        .iter()
        .filter(|s| !["claimed", "in_progress", "testing"].contains(s))
    {
        let task = executing(&store, Some(1))?;
        db.write()?.execute(
            "UPDATE tasks SET status = ?2, updated_at = 1 WHERE id = ?1",
            params![task.id, status],
        )?;
    }
    let parent = executing(&store, Some(1))?;
    let child = store.create_full(NewTask {
        channel: "#one".into(),
        title: "Child".into(),
        assignee: Some("worker".into()),
        parent_task_id: Some(parent.id.clone()),
        timeout_seconds: Some(1),
        ..Default::default()
    })?;
    store.set_status(&child.id, "assigned", "in_progress", "worker", None, None)?;
    store.ask(&parent.id, "worker", "Decision")?;
    assert!(store
        .recover_timeouts(unix_timestamp() + 100000)?
        .is_empty());
    assert_eq!(store.get(&child.id)?.unwrap().status, "blocked");
    store.answer(&parent.id, "pm", "Resume")?;
    let resumed = store.get(&child.id)?.unwrap();
    assert_eq!(resumed.status, "in_progress");
    assert!(resumed.execution_heartbeat_at.is_some());
    Ok(())
}

#[test]
fn expired_execution_cannot_transition_before_sweep_and_testing_gets_fresh_interval() -> Result<()>
{
    let db = crate::store::open(":memory:")?;
    let store = TaskStore::new(db.clone(), 60);
    let task = executing(&store, Some(60))?;
    db.write()?.execute(
        "UPDATE tasks SET execution_heartbeat_at = ?2 WHERE id = ?1",
        params![task.id, unix_timestamp() - 50],
    )?;
    let testing = store.set_status(&task.id, "in_progress", "testing", "worker", None, None)?;
    assert_eq!(testing.execution_heartbeat_at, Some(testing.updated_at));
    db.write()?.execute(
        "UPDATE tasks SET execution_heartbeat_at = 1 WHERE id = ?1",
        [&task.id],
    )?;
    assert!(store
        .set_status(
            &task.id,
            "testing",
            "review",
            "worker",
            Some("Passed"),
            None
        )
        .is_err());
    assert!(store.heartbeat(&task.id, "worker").is_err());
    assert_eq!(store.recover_timeouts(unix_timestamp())?.len(), 1);
    Ok(())
}

#[test]
fn timeout_persists_over_restart_and_concurrent_sweep_is_exactly_once() -> Result<()> {
    let path = std::env::temp_dir().join(format!("aircd-timeout-{}.db", Uuid::new_v4()));
    let id;
    {
        let db = crate::store::open(path.to_str().unwrap())?;
        let store = TaskStore::new(db.clone(), 60);
        id = executing(&store, Some(10))?.id;
        db.write()?.execute(
            "UPDATE tasks SET execution_heartbeat_at = NULL, updated_at = 1 WHERE id = ?1",
            [&id],
        )?;
    }
    {
        let db = crate::store::open(path.to_str().unwrap())?;
        let store = TaskStore::new(db, 60);
        assert_eq!(store.get(&id)?.unwrap().timeout_seconds, Some(10));
        assert_eq!(store.get(&id)?.unwrap().execution_heartbeat_at, Some(1));
        let other = store.clone();
        let thread = std::thread::spawn(move || other.recover_timeouts(unix_timestamp()));
        let ours = store.recover_timeouts(unix_timestamp())?;
        assert_eq!(ours.len() + thread.join().unwrap()?.len(), 1);
        assert_eq!(
            store
                .events(&id)?
                .iter()
                .filter(|e| e.action == "timeout")
                .count(),
            1
        );
    }
    std::fs::remove_file(path)?;
    Ok(())
}

#[test]
fn timeout_and_claim_completion_race_preserves_winning_state() -> Result<()> {
    let db = crate::store::open(":memory:")?;
    let store = TaskStore::new(db, 60);
    for _ in 0..8 {
        let task = store.create("#one", "Raced claim")?;
        let claimed = store.claim(&task.id, "worker")?;
        let other = store.clone();
        let id = task.id.clone();
        let thread = std::thread::spawn(move || other.done(&id, "worker"));
        let recovered = store.recover_timeouts(claimed.lease_expires_at.unwrap())?;
        let completion = thread.join().unwrap();
        let final_task = store.get(&task.id)?.unwrap();
        if completion.is_ok() {
            assert!(recovered.is_empty());
            assert_eq!(final_task.status, "done");
        } else {
            assert_eq!(recovered.len(), 1);
            assert_eq!(final_task.status, "open");
            assert!(final_task.claimed_by.is_none());
        }
    }
    Ok(())
}

#[test]
fn checkpoint_active_leases_extends_active_tasks_and_skips_expired() -> Result<()> {
    let db = crate::store::open(":memory:")?;
    let store = TaskStore::new(db, 60);

    let claimed_live = store.create("#one", "Claimed live")?;
    let claimed_live = store.claim(&claimed_live.id, "worker-a")?;
    let claimed_expired = store.create("#one", "Claimed expired")?;
    let claimed_expired = store.claim(&claimed_expired.id, "worker-b")?;
    let in_progress_live = executing(&store, Some(120))?;
    let testing_live = {
        let t = executing(&store, Some(120))?;
        store.set_status(&t.id, "in_progress", "testing", "worker", None, None)?
    };
    let in_progress_expired = executing(&store, Some(10))?;
    let open_task = store.create("#one", "Still open")?;

    let checkpoint_at = claimed_live.lease_expires_at.unwrap() - 5;
    store.db.write()?.execute(
        "UPDATE tasks SET lease_expires_at = ?1 WHERE id = ?2",
        rusqlite::params![checkpoint_at - 1, &claimed_expired.id],
    )?;
    store.db.write()?.execute(
        "UPDATE tasks SET execution_heartbeat_at = ?1 WHERE id = ?2",
        rusqlite::params![checkpoint_at - 20, &in_progress_expired.id],
    )?;
    store.db.write()?.execute(
        "UPDATE tasks SET execution_heartbeat_at = ?1 WHERE id IN (?2, ?3)",
        rusqlite::params![checkpoint_at - 15, &in_progress_live.id, &testing_live.id],
    )?;

    let checkpointed = store.checkpoint_active_leases(checkpoint_at, 300)?;
    let ids: Vec<&str> = checkpointed.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ids.len(), 3);
    assert!(ids.contains(&claimed_live.id.as_str()));
    assert!(ids.contains(&in_progress_live.id.as_str()));
    assert!(ids.contains(&testing_live.id.as_str()));
    assert!(!ids.contains(&claimed_expired.id.as_str()));
    assert!(!ids.contains(&in_progress_expired.id.as_str()));
    assert!(!ids.contains(&open_task.id.as_str()));

    let updated_claim = store.get(&claimed_live.id)?.unwrap();
    assert_eq!(updated_claim.lease_expires_at, Some(checkpoint_at + 300));
    let updated_in_prog = store.get(&in_progress_live.id)?.unwrap();
    assert_eq!(updated_in_prog.execution_heartbeat_at, Some(checkpoint_at));
    let updated_testing = store.get(&testing_live.id)?.unwrap();
    assert_eq!(updated_testing.execution_heartbeat_at, Some(checkpoint_at));

    let events = store.events(&in_progress_live.id)?;
    assert_eq!(events.last().unwrap().action, "lease_checkpoint");
    let metrics = crate::task_metrics::compute(&updated_in_prog, &events, checkpoint_at + 10);
    assert!(metrics.time_in_status["in_progress"] >= 10);

    // Checkpointed claimed task is not immediately expired after checkpoint_at + 10.
    assert!(store.recover_timeouts(checkpoint_at + 10)?.iter().all(|r| r.task.id != claimed_live.id));
    Ok(())
}
