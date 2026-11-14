use super::*;

fn task(store: &TaskStore, parent: Option<&str>, assignee: Option<&str>) -> Result<Task> {
    store.create_full(NewTask {
        channel: "#one".into(),
        title: "Work".into(),
        assignee: assignee.map(str::to_string),
        parent_task_id: parent.map(str::to_string),
        branch: Some("feat/work".into()),
        ..Default::default()
    })
}

#[test]
fn cancellation_cascades_all_active_phases_and_cleans_questions_and_leases() -> Result<()> {
    let db = crate::store::open(":memory:")?;
    let store = TaskStore::new(db.clone(), 60);
    let root = task(&store, None, Some("lead"))?;
    let mut children = Vec::new();
    for status in ALL_STATUSES {
        let child = task(&store, Some(&root.id), Some("worker"))?;
        // Build every phase, including historical combinations, without extra events.
        db.write()?.execute("UPDATE tasks SET status = ?2, claimed_by = 'worker', lease_expires_at = 9999999999 WHERE id = ?1",
            params![child.id, status])?;
        children.push((child.id, *status));
    }
    let nested = task(&store, Some(&children[0].0), Some("worker"))?;
    store.ask(&nested.id, "worker", "Need a decision")?;
    store.escalate(&nested.id, "pm", None)?;
    let terminal_snapshots: Vec<_> = children
        .iter()
        .filter(|(_, s)| TERMINAL_STATUSES.contains(s))
        .map(|(id, _)| Ok((id.clone(), serde_json::to_value(store.get(id)?)?)))
        .collect::<Result<_>>()?;
    let update = store.set_status_with_round(
        &root.id,
        "assigned",
        "cancelled",
        "pm",
        Some("Stop"),
        None,
        None,
    )?;
    assert_eq!(
        update.cascaded.len(),
        ALL_STATUSES.len() - TERMINAL_STATUSES.len() + 1
    );
    assert!(store.pending_question(&nested.id)?.is_none());
    assert!(store.questions(None, "escalated")?.is_empty());
    assert_eq!(store.questions(None, "cancelled")?.len(), 1);
    assert!(store.inbox("worker")?.is_empty());
    assert!(store.review_queue("#one")?.is_empty());
    for change in update.cascaded {
        assert_eq!(change.task.status, "cancelled");
        assert!(change.task.claimed_by.is_none());
        assert!(change.task.lease_expires_at.is_none());
        assert!(change.task.review_round.is_none());
        assert_eq!(change.task.branch.as_deref(), Some("feat/work"));
        let events = store.events(&change.task.id)?;
        let event = events.last().unwrap();
        assert_eq!(event.action, "parent_cancel");
        assert_eq!(event.actor_id, "pm");
        assert!(event.note.as_deref().unwrap().contains(&root.id));
    }
    for (id, snapshot) in terminal_snapshots {
        assert_eq!(serde_json::to_value(store.get(&id)?)?, snapshot);
        assert!(store.events(&id)?.is_empty());
    }
    assert!(store.answer(&nested.id, "pm", "Too late").is_err());
    Ok(())
}

#[test]
fn parent_answer_restores_only_inherited_pauses_and_requires_fresh_reviews_and_claims() -> Result<()>
{
    let db = crate::store::open(":memory:")?;
    let store = TaskStore::new(db, 60);
    let root = task(&store, None, Some("lead"))?;
    let own = task(&store, Some(&root.id), Some("worker"))?;
    let grandchild = task(&store, Some(&own.id), Some("peer"))?;
    store.ask(&own.id, "worker", "Own blocker")?;
    let review = task(&store, Some(&root.id), Some("worker"))?;
    let round = ReviewRound {
        revision: "old".into(),
        group: "team".into(),
        assignee: "worker".into(),
        commit: "1".repeat(40),
        branch: "feat/work".into(),
        evidence: "Passed".into(),
        peer_reviewers: vec!["peer".into()],
        quorum: 1,
        votes: Vec::new(),
    };
    store.set_status_with_round(
        &review.id,
        "assigned",
        "review",
        "worker",
        None,
        None,
        Some(&round),
    )?;
    store.peer_vote(
        &review.id,
        "old",
        "peer",
        "approve",
        "Reviewed",
        &["peer".into()],
    )?;
    assert_eq!(store.get(&review.id)?.unwrap().status, "super_review");
    let claimed = task(&store, Some(&root.id), None)?;
    store.claim(&claimed.id, "lease-worker")?;
    let (update, _) = store.ask(&root.id, "lead", "Parent blocker")?;
    assert_eq!(update.cascaded.len(), 2); // Own blocker and its inherited pause already existed.
    assert!(update
        .cascaded
        .iter()
        .any(|c| c.previous_holder.as_deref() == Some("lease-worker")));
    assert!(store.answer(&own.id, "pm", "Cannot bypass parent").is_err());
    assert_eq!(store.pending_question(&own.id)?.unwrap().status, "open");
    let (update, _) = store.answer(&root.id, "pm", "Continue")?;
    assert_eq!(update.cascaded.len(), 2);
    assert_eq!(store.get(&review.id)?.unwrap().status, "testing");
    assert!(store.get(&review.id)?.unwrap().review_round.is_none());
    assert!(store
        .peer_vote(
            &review.id,
            "old",
            "peer",
            "approve",
            "Stale",
            &["peer".into()]
        )
        .is_err());
    assert_eq!(store.get(&claimed.id)?.unwrap().status, "open");
    assert!(store.done(&claimed.id, "lease-worker").is_err());
    assert_eq!(store.get(&own.id)?.unwrap().status, "blocked");
    assert_eq!(store.get(&grandchild.id)?.unwrap().status, "blocked");
    let (update, _) = store.answer(&own.id, "pm", "Independent decision")?;
    assert_eq!(update.cascaded.len(), 1);
    assert_eq!(store.get(&grandchild.id)?.unwrap().status, "assigned");
    Ok(())
}

#[test]
fn failed_child_audit_rolls_back_parent_question_answer_and_cancellation() -> Result<()> {
    let db = crate::store::open(":memory:")?;
    let store = TaskStore::new(db.clone(), 60);
    let root = task(&store, None, Some("lead"))?;
    let child = task(&store, Some(&root.id), Some("worker"))?;
    db.write()?.execute_batch(
        "CREATE TRIGGER reject_cascade BEFORE INSERT ON task_events
        WHEN NEW.action LIKE 'parent_%' BEGIN SELECT RAISE(ABORT, 'forced failure'); END;",
    )?;
    assert!(store.ask(&root.id, "lead", "Will roll back").is_err());
    assert!(store.pending_question(&root.id)?.is_none());
    assert_eq!(store.get(&root.id)?.unwrap().status, "assigned");
    assert_eq!(store.get(&child.id)?.unwrap().status, "assigned");
    assert!(store.events(&root.id)?.is_empty());
    db.write()?.execute("DROP TRIGGER reject_cascade", [])?;
    store.ask(&root.id, "lead", "Real question")?;
    db.write()?.execute_batch(
        "CREATE TRIGGER reject_cascade BEFORE INSERT ON task_events
        WHEN NEW.action LIKE 'parent_%' BEGIN SELECT RAISE(ABORT, 'forced failure'); END;",
    )?;
    assert!(store.answer(&root.id, "pm", "Will roll back").is_err());
    assert_eq!(store.pending_question(&root.id)?.unwrap().status, "open");
    assert!(store
        .set_status(&root.id, "blocked", "cancelled", "pm", None, None)
        .is_err());
    assert_eq!(store.get(&root.id)?.unwrap().status, "blocked");
    assert_eq!(store.get(&child.id)?.unwrap().status, "blocked");
    assert_eq!(store.events(&root.id)?.len(), 1);
    db.write()?.execute("DROP TRIGGER reject_cascade", [])?;
    store.answer(&root.id, "pm", "Continue")?;
    assert_eq!(store.get(&child.id)?.unwrap().status, "assigned");
    Ok(())
}

#[test]
fn ancestor_guards_reject_bypasses_and_legacy_cycles_terminate() -> Result<()> {
    let db = crate::store::open(":memory:")?;
    let store = TaskStore::new(db.clone(), 60);
    let root = task(&store, None, Some("lead"))?;
    let child = task(&store, Some(&root.id), Some("worker"))?;
    store.ask(&root.id, "lead", "Wait")?;
    let snapshot = serde_json::to_value(store.get(&child.id)?)?;
    assert!(task(&store, Some(&root.id), None).is_err());
    assert!(task(&store, Some(&child.id), None).is_err());
    assert!(store.assign(&child.id, Some("peer"), "pm").is_err());
    assert!(store.claim(&child.id, "worker").is_err());
    assert!(store.release(&child.id, "worker").is_err());
    assert!(store.done(&child.id, "worker").is_err());
    assert!(store.ask(&child.id, "worker", "Bypass").is_err());
    assert!(store
        .set_status(&child.id, "blocked", "testing", "worker", None, None)
        .is_err());
    assert_eq!(serde_json::to_value(store.get(&child.id)?)?, snapshot);
    // Corrupt historical links must neither loop forever nor update the root twice.
    db.write()?.execute(
        "UPDATE tasks SET parent_task_id = ?2 WHERE id = ?1",
        params![root.id, child.id],
    )?;
    let update =
        store.set_status_with_round(&root.id, "blocked", "cancelled", "pm", None, None, None)?;
    assert_eq!(update.cascaded.len(), 1);
    assert!(task(&store, Some(&child.id), None).is_err());
    Ok(())
}

#[test]
fn inherited_pauses_survive_reopen_and_terminal_branches_still_reach_active_descendants(
) -> Result<()> {
    let path = std::env::temp_dir().join(format!("aircd-cascade-{}.sqlite3", Uuid::new_v4()));
    let (root_id, child_id, terminal_id, nested_id);
    {
        let db = crate::store::open(&path)?;
        let store = TaskStore::new(db.clone(), 60);
        root_id = task(&store, None, Some("lead"))?.id;
        child_id = task(&store, Some(&root_id), Some("worker"))?.id;
        terminal_id = task(&store, Some(&root_id), Some("worker"))?.id;
        nested_id = task(&store, Some(&terminal_id), Some("worker"))?.id;
        db.write()?.execute(
            "UPDATE tasks SET status = 'merged' WHERE id = ?1",
            [&terminal_id],
        )?;
        store.ask(&root_id, "lead", "Persisted pause")?;
        assert_eq!(store.get(&nested_id)?.unwrap().status, "blocked");
    }
    {
        let db = crate::store::open(&path)?;
        let store = TaskStore::new(db, 60);
        store.answer(&root_id, "pm", "After restart")?;
        assert_eq!(store.get(&child_id)?.unwrap().status, "assigned");
        assert_eq!(store.get(&nested_id)?.unwrap().status, "assigned");
        assert_eq!(store.get(&terminal_id)?.unwrap().status, "merged");
        store.set_status(&root_id, "assigned", "cancelled", "pm", None, None)?;
        assert_eq!(store.get(&nested_id)?.unwrap().status, "cancelled");
        assert_eq!(store.get(&terminal_id)?.unwrap().status, "merged");
    }
    std::fs::remove_file(path)?;
    Ok(())
}

#[test]
fn concurrent_child_creation_and_parent_cancellation_cannot_leave_active_orphans() -> Result<()> {
    let db = crate::store::open(":memory:")?;
    let store = TaskStore::new(db, 60);
    for _ in 0..12 {
        let root = task(&store, None, Some("lead"))?;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let creator = store.clone();
        let parent_id = root.id.clone();
        let ready = barrier.clone();
        let worker = std::thread::spawn(move || {
            ready.wait();
            task(&creator, Some(&parent_id), Some("worker"))
        });
        barrier.wait();
        store.set_status(&root.id, "assigned", "cancelled", "pm", None, None)?;
        if let Ok(child) = worker.join().unwrap() {
            assert_eq!(store.get(&child.id)?.unwrap().status, "cancelled");
        }
        assert!(store
            .children(&root.id)?
            .iter()
            .all(|t| t.status == "cancelled"));
    }
    Ok(())
}
