use super::*;
use aionui_db::SqliteConversationRepository;

async fn setup(
    default_mode: &str,
    snapshot_effort: Option<&str>,
    team: bool,
    with_snapshot: bool,
) -> (
    ConversationService,
    Arc<SqliteConversationRepository>,
    Arc<MockTaskManager>,
    ConversationRow,
) {
    let db = init_database_memory().await.unwrap();
    seed_test_user(db.pool(), "user_1").await;
    let repo = Arc::new(SqliteConversationRepository::new(db.pool().clone()));
    let task_mgr = Arc::new(MockTaskManager::new());
    let svc = new_service(repo.clone(), task_mgr.clone());
    let mut extra = json!({
        "workspace": ensure_test_workspace_path(),
        "thought_level": "low",
        "session_mode": "default"
    });
    if team {
        extra["teamId"] = json!("team-1");
        extra["slot_id"] = json!("worker-1");
        extra["role"] = json!("teammate");
    }
    let row = ConversationRow {
        id: "effort-rebuild".into(),
        user_id: "user_1".into(),
        name: "effort rebuild".into(),
        r#type: "aionrs".into(),
        extra: extra.to_string(),
        model: None,
        status: Some("finished".into()),
        source: Some("aionui".into()),
        channel_chat_id: None,
        pinned: false,
        pinned_at: None,
        created_at: 1,
        updated_at: 1,
        project_id: None,
        folder_id: None,
        name_source: None,
    };
    repo.create(&row).await.unwrap();
    if with_snapshot {
        upsert_test_assistant_definition_with_thought_level(
            &SqliteAssistantDefinitionRepository::new(db.pool().clone()),
            "asstdef-effort",
            "assistant-effort",
            "agent-effort",
            "auto",
            "fixed",
            default_mode,
        )
        .await;
        repo.upsert_assistant_snapshot(
            &row.user_id,
            &UpsertConversationAssistantSnapshotParams {
                conversation_id: &row.id,
                assistant_definition_id: "asstdef-effort",
                assistant_id: "assistant-effort",
                assistant_source: "builtin",
                agent_id: "agent-effort",
                rules_content: "",
                default_model_mode: "auto",
                resolved_model_id: None,
                default_permission_mode: "fixed",
                resolved_permission_value: Some("yolo"),
                default_thought_level_mode: default_mode,
                resolved_thought_level_value: snapshot_effort,
                default_skills_mode: "auto",
                resolved_skill_ids: "[]",
                resolved_disabled_builtin_skill_ids: "[]",
                default_mcps_mode: "auto",
                resolved_mcp_ids: "[]",
            },
        )
        .await
        .unwrap();
    }
    (svc, repo, task_mgr, row)
}

fn new_service(repo: Arc<SqliteConversationRepository>, task_mgr: Arc<MockTaskManager>) -> ConversationService {
    ConversationService::new(
        std::env::temp_dir(),
        Arc::new(MockBroadcaster::new()),
        Arc::new(FixedSkillResolver { names: vec![] }),
        task_mgr,
        repo,
        Arc::new(StubAgentMetadataRepo),
        Arc::new(StubAcpSessionRepo::default()),
    )
}

fn assert_effort(options: &BuildTaskOptions, expected: &str) {
    let AgentSessionKind::Aionrs(ctx) = &options.context.kind else {
        panic!("expected Aionrs build options");
    };
    assert_eq!(ctx.config.thought_level.as_deref(), Some(expected));
    assert_eq!(ctx.config.session_mode.as_deref(), Some("default"));
}

#[tokio::test]
async fn aionrs_effort_rebuild_restores_runtime_choice_from_sqlite_for_auto_and_fixed_defaults() {
    for mode in ["auto", "fixed"] {
        let (svc, repo, task_mgr, row) = setup(mode, Some("low"), false, true).await;
        task_mgr.insert_agent(&row.id, AgentInstance::Mock(Arc::new(MockAgent::new(&row.id))));
        svc.set_config_option(
            &row.user_id,
            &row.id,
            "reasoning_effort",
            SetConfigOptionRequest { value: "high".into() },
        )
        .await
        .unwrap();
        let snapshot = repo
            .get_assistant_snapshot(&row.user_id, &row.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.resolved_thought_level_value.as_deref(), Some("high"));
        let stored_row = repo.get(&row.user_id, &row.id).await.unwrap().unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&stored_row.extra).unwrap()["thought_level"],
            "low"
        );

        // A fresh service has no live task or runtime selection to lean on.
        let rebuilt = new_service(repo, Arc::new(MockTaskManager::new()));
        assert_effort(&rebuilt.build_task_options(&stored_row).await.unwrap(), "high");
        assert_effort(
            &rebuilt.build_task_options_for_runtime(&stored_row, None).await.unwrap(),
            "high",
        );
    }
}

#[tokio::test]
async fn aionrs_effort_rebuild_preserves_legacy_and_team_seeds() {
    for (effort, team, with_snapshot) in [
        (None, false, false),
        (None, false, true),
        (Some(""), false, true),
        (Some("  "), false, true),
        (Some("high"), true, true),
    ] {
        let (svc, _, _, row) = setup("auto", effort, team, with_snapshot).await;
        assert_effort(&svc.build_task_options(&row).await.unwrap(), "low");
        assert_effort(&svc.build_task_options_for_runtime(&row, None).await.unwrap(), "low");
    }
}

#[tokio::test]
async fn aionrs_effort_rebuild_model_switch_discards_previous_runtime_strength() {
    let (svc, repo, tasks, row) = setup("auto", Some("high"), false, true).await;
    let request = serde_json::from_value(json!({
        "model": {"provider_id": "custom", "model": "unknown-model"}
    }))
    .unwrap();
    let tasks: Arc<dyn IWorkerTaskManager> = tasks;
    svc.update(&row.user_id, &row.id, request, &tasks).await.unwrap();
    let stored = repo.get(&row.user_id, &row.id).await.unwrap().unwrap();
    assert_effort(&svc.build_task_options(&stored).await.unwrap(), "none");
    let snapshot = repo
        .get_assistant_snapshot(&row.user_id, &row.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.resolved_thought_level_value.as_deref(), Some("none"));
    let request = serde_json::from_value(json!({
        "model": {"provider_id": "custom", "model": "another-model"},
        "extra": {"thought_level": "high"}
    }))
    .unwrap();
    svc.update(&row.user_id, &row.id, request, &tasks).await.unwrap();
    let stored = repo.get(&row.user_id, &row.id).await.unwrap().unwrap();
    assert_effort(&svc.build_task_options(&stored).await.unwrap(), "high");
    let same_model = serde_json::from_value(json!({
        "model": {"provider_id": "custom", "model": "another-model"}
    }))
    .unwrap();
    svc.update(&row.user_id, &row.id, same_model, &tasks).await.unwrap();
    let stored = repo.get(&row.user_id, &row.id).await.unwrap().unwrap();
    assert_effort(&svc.build_task_options(&stored).await.unwrap(), "high");
}
