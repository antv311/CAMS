use super::*;
use crate::store::{GroupInput, GroupStore};
use crate::tasks::{NewTask, TaskStore};

fn setup() -> Result<(WakeManager, GroupStore, WakeConfig, WakeContext)> {
    let db = crate::store::open(":memory:")?;
    for channel in ["#one", "#two"] {
        crate::projects::ProjectStore::new(db.clone()).create(channel, channel, None, "owner")?;
    }
    crate::rooms::RoomStore::new(db.clone()).open("#one", None, "owner")?;
    let wakes = WakeManager::new(db.clone());
    let cfg = wakes.upsert(
        "worker",
        WakeConfigInput {
            command: "unused".into(),
            prompt: Some("ACCOUNT {nick} {role}".into()),
            enabled: Some(true),
            ..Default::default()
        },
    )?;
    let ctx = WakeContext {
        principal_id: "worker".into(),
        nick: "Worker".into(),
        channel: "#one-s1".into(),
        reason: "literal {task_id} {prompt} café".into(),
        repo_path: Some("repo/{nick}".into()),
        ..Default::default()
    };
    Ok((wakes, GroupStore::new(db), cfg, ctx))
}

fn definition(name: &str) -> Result<GroupInput> {
    Ok(serde_json::from_value(serde_json::json!({
        "name": name,
        "lead": {"template": "claude", "model": "opus", "nick": "Lead"},
        "workers": [{"template": "local", "model": "code", "nick": "@worker"}],
        "lead_prompt": "LEAD {nick} {group} {role}: split, test, review",
        "worker_prompt": "WORKER {nick} {group} {role} {project} {channel}\n{task_id}: {task_title}\n{task_body}\n{parent_task_id} {reviewer}\n{reason}\n{repo_path}\n{unknown}"
    }))?)
}

fn vars(
    wakes: &WakeManager,
    cfg: &WakeConfig,
    ctx: &WakeContext,
) -> Result<BTreeMap<&'static str, String>> {
    Ok(wakes
        .launch_vars(cfg, ctx, "http://hub")?
        .into_iter()
        .collect())
}

#[test]
fn group_prompt_selects_named_roles_in_attached_projects_and_rooms() -> Result<()> {
    let (wakes, groups, cfg, mut ctx) = setup()?;
    groups.upsert(definition("dev")?)?;
    assert_eq!(vars(&wakes, &cfg, &ctx)?["prompt"], "ACCOUNT Worker member");
    groups.attach("dev", "#one")?;
    let worker = vars(&wakes, &cfg, &ctx)?;
    assert!(worker["prompt"].starts_with("WORKER Worker dev group worker #one #one-s1"));
    assert_eq!(worker["group"], "dev");
    ctx.nick = "lEaD".into();
    ctx.principal_id = "lead".into();
    assert_eq!(
        vars(&wakes, &cfg, &ctx)?["prompt"],
        "LEAD lEaD dev group lead: split, test, review"
    );
    ctx.channel = "#two".into();
    assert_eq!(vars(&wakes, &cfg, &ctx)?["prompt"], "ACCOUNT lEaD member");
    groups.attach("dev", "#two")?;
    assert!(vars(&wakes, &cfg, &ctx)?["prompt"].starts_with("LEAD"));
    ctx.channel = "@dm:lead:worker".into();
    assert_eq!(vars(&wakes, &cfg, &ctx)?["group"], "");
    Ok(())
}

#[test]
fn group_prompt_interpolates_only_event_task_and_preserves_literal_values() -> Result<()> {
    let (wakes, groups, cfg, mut ctx) = setup()?;
    groups.upsert(definition("dev")?)?;
    groups.attach("dev", "#one")?;
    let task = TaskStore::new(wakes.db.clone(), 0).create_full(NewTask {
        channel: "#one".into(),
        title: "Implement {task_body}".into(),
        body: Some("Keep {nick} {prompt} {role} and café; $(literal)".into()),
        parent_task_id: Some("parent-1".into()),
        reviewer: Some("lead".into()),
        ..Default::default()
    })?;
    ctx.task_id = Some(task.id.clone());
    let values = vars(&wakes, &cfg, &ctx)?;
    assert_eq!(values["prompt"], format!(
        "WORKER Worker dev group worker #one #one-s1\n{}: Implement {{task_body}}\nKeep {{nick}} {{prompt}} {{role}} and café; $(literal)\nparent-1 lead\nliteral {{task_id}} {{prompt}} café\nrepo/{{nick}}\n{{unknown}}", task.id));
    ctx.channel = "#two".into();
    let values = vars(&wakes, &cfg, &ctx)?;
    assert_eq!(values["task_body"], "");
    assert_eq!(values["task_title"], "");
    ctx.task_id = Some("missing".into());
    assert_eq!(vars(&wakes, &cfg, &ctx)?["parent_task_id"], "");
    Ok(())
}

#[test]
fn group_prompt_fallbacks_preserve_account_pm_defaults_and_empty_templates() -> Result<()> {
    let (wakes, groups, mut cfg, mut ctx) = setup()?;
    let mut group = definition("dev")?;
    group.worker_prompt = None;
    groups.upsert(group.clone())?;
    groups.attach("dev", "#one")?;
    assert_eq!(
        vars(&wakes, &cfg, &ctx)?["prompt"],
        "ACCOUNT Worker group worker"
    );
    cfg.prompt = None;
    assert!(vars(&wakes, &cfg, &ctx)?["prompt"].starts_with("You are @Worker on"));
    ctx.is_pm = true;
    assert!(vars(&wakes, &cfg, &ctx)?["prompt"].starts_with("You are @Worker, the project manager"));
    group.worker_prompt = Some(String::new());
    groups.upsert(group.clone())?;
    assert_eq!(vars(&wakes, &cfg, &ctx)?["prompt"], "");
    group.workers[0].nick = None;
    group.workers[0].count = 3;
    groups.upsert(group)?;
    assert_eq!(vars(&wakes, &cfg, &ctx)?["role"], "project manager");
    assert_eq!(vars(&wakes, &cfg, &ctx)?["group"], "");
    Ok(())
}

#[test]
fn group_prompt_templates_survive_reopen_and_replacement_keeps_attachments() -> Result<()> {
    let path = std::env::temp_dir().join(format!("aircd-prompt-{}.sqlite3", Uuid::new_v4()));
    let result = (|| -> Result<()> {
        {
            let db = crate::store::open(&path)?;
            crate::projects::ProjectStore::new(db.clone()).create("#one", "One", None, "owner")?;
            let groups = GroupStore::new(db);
            groups.upsert(definition("dev")?)?;
            groups.attach("dev", "#one")?;
        }
        let wakes = WakeManager::new(crate::store::open(&path)?);
        let cfg = wakes.upsert(
            "worker",
            WakeConfigInput {
                command: "unused".into(),
                ..Default::default()
            },
        )?;
        let ctx = WakeContext {
            nick: "worker".into(),
            channel: "#one".into(),
            ..Default::default()
        };
        assert!(vars(&wakes, &cfg, &ctx)?["prompt"].starts_with("WORKER"));
        let groups = GroupStore::new(wakes.db.clone());
        let mut group = definition("dev")?;
        group.worker_prompt = Some("Updated {group}".into());
        groups.upsert(group)?;
        assert_eq!(vars(&wakes, &cfg, &ctx)?["prompt"], "Updated dev");
        groups.delete("dev")?;
        assert!(vars(&wakes, &cfg, &ctx)?["prompt"].starts_with("You are @worker on"));
        Ok(())
    })();
    let _ = std::fs::remove_file(path);
    result
}

#[tokio::test]
async fn group_prompt_ambiguous_bindings_record_failure_without_launching() -> Result<()> {
    let (wakes, groups, cfg, ctx) = setup()?;
    for name in ["a", "b"] {
        groups.upsert(definition(name)?)?;
        groups.attach(name, "#one")?;
    }
    assert!(vars(&wakes, &cfg, &ctx)
        .unwrap_err()
        .to_string()
        .contains("ambiguous group role"));
    wakes.run_once(&cfg, &ctx).await?;
    let runs = wakes.runs(Some("worker"), 10)?;
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, "failed");
    assert!(runs[0]
        .log_tail
        .as_deref()
        .unwrap()
        .contains("ambiguous group role"));
    groups.delete("b")?;
    let mut group = definition("a")?;
    group.lead.nick = Some("worker".into());
    groups.upsert(group)?;
    assert!(vars(&wakes, &cfg, &ctx).is_err());
    Ok(())
}

#[test]
fn expand_is_single_pass_and_keeps_unknown_or_incomplete_placeholders() {
    assert_eq!(
        expand(
            r#"{"nick": "{nick}", "other": {unknown}}"#,
            &[("nick", "worker")]
        ),
        r#"{"nick": "worker", "other": {unknown}}"#
    );
    assert_eq!(
        expand(
            "é {reason} {nick} {missing} {open",
            &[
                ("reason", "{nick} {prompt}"),
                ("nick", "worker"),
                ("prompt", "secret")
            ]
        ),
        "é {nick} {prompt} worker {missing} {open"
    );
    assert_eq!(
        expand("{prompt}", &[("prompt", "{nick}"), ("nick", "worker")]),
        "{nick}"
    );
}

// A real, portable child process for the launch test, using this test binary.
#[test]
fn prompt_probe_child() {
    if std::env::var("AIRCD_PROMPT_TEST_PROBE").as_deref() == Ok("1") {
        println!(
            "PROBE {}",
            serde_json::json!({
                "prompt": std::env::var("AIRCD_PROMPT_TEST_TEXT").unwrap(),
                "role": std::env::var("AIRCD_PROMPT_TEST_ROLE").unwrap(),
                "session": std::env::var("AIRCD_PROMPT_TEST_SESSION").unwrap(),
                "reason": std::env::var("AIRCD_WAKE_REASON").unwrap()
            })
        );
        println!("session id: prompt-session");
    }
}

#[tokio::test]
async fn group_prompt_reaches_child_and_refreshes_without_changing_sessions() -> Result<()> {
    let (wakes, groups, _, ctx) = setup()?;
    let mut group = definition("dev")?;
    group.worker_prompt = Some("First {reason} {nick}".into());
    groups.upsert(group.clone())?;
    groups.attach("dev", "#one")?;
    let cfg = wakes.upsert(
        "worker",
        WakeConfigInput {
            command: std::env::current_exe()?.to_string_lossy().into_owned(),
            args: vec![
                "--exact".into(),
                "wake::prompt_tests::prompt_probe_child".into(),
                "--nocapture".into(),
            ],
            env: BTreeMap::from([
                ("AIRCD_PROMPT_TEST_PROBE".into(), Some("1".into())),
                ("AIRCD_PROMPT_TEST_TEXT".into(), Some("{prompt}".into())),
                ("AIRCD_PROMPT_TEST_ROLE".into(), Some("{role}".into())),
                (
                    "AIRCD_PROMPT_TEST_SESSION".into(),
                    Some("{session_id}".into()),
                ),
            ]),
            session_mode: Some("resume".into()),
            session_scope: Some("channel".into()),
            session_marker: Some("session id:".into()),
            timeout_seconds: Some(10),
            ..Default::default()
        },
    )?;
    wakes.run_once(&cfg, &ctx).await?;
    let check = |expected: &str, session: &str| -> Result<()> {
        let runs = wakes.runs(Some("worker"), 10)?;
        // Runs can share a second; find the expected child output rather than rely on tie order.
        let output = runs
            .iter()
            .filter_map(|run| run.log_tail.as_deref())
            .flat_map(str::lines)
            .filter_map(|line| line.strip_prefix("PROBE "))
            .map(serde_json::from_str::<serde_json::Value>)
            .collect::<serde_json::Result<Vec<_>>>()?;
        assert!(output.iter().any(|value| value["prompt"] == expected
            && value["role"] == "group worker"
            && value["session"] == session
            && value["reason"] == ctx.reason));
        assert!(runs.iter().all(|run| run.status == "ok"));
        Ok(())
    };
    check("First literal {task_id} {prompt} café Worker", "")?;
    assert_eq!(
        wakes.session("worker", "#one-s1")?.as_deref(),
        Some("prompt-session")
    );
    group.worker_prompt = Some("Second {group} {nick}".into());
    groups.upsert(group)?;
    wakes.run_once(&cfg, &ctx).await?;
    check("Second dev Worker", "prompt-session")?;
    Ok(())
}
