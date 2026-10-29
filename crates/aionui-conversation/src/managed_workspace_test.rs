use super::*;
use aionui_api_types::{CreateWorkspaceRequest, UpdateWorkspaceRequest, WorkspaceRootRequest};
use aionui_project::canonical::to_file_uri;

fn managed_request(project_id: &str) -> CreateConversationRequest {
    serde_json::from_value(json!({"type":"aionrs", "extra": {
        "workspace_project_id": project_id,
        "workspace": "Z:/stale/client/path"
    }}))
    .unwrap()
}

#[tokio::test]
async fn managed_workspace_binding_requires_project_service() {
    let (service, _, repo, _) = make_service();
    let req = serde_json::from_value(json!({
        "type": "aionrs",
        "extra": {"workspace_project_id": "missing-workspace"}
    }))
    .unwrap();
    let error = service.create("user_1", req).await.unwrap_err();
    assert!(error.to_string().contains("workspace"), "{error}");
    assert!(repo.rows.lock().unwrap().is_empty());
}

#[tokio::test]
async fn managed_workspace_binding_rejects_external_agent() {
    let (service, _, _, _) = make_service();
    let mut req = make_create_req();
    req.extra["workspace_project_id"] = json!("workspace-1");
    let error = service.create("user_1", req).await.unwrap_err();
    assert!(error.to_string().contains("aionrs"), "{error}");
}

#[tokio::test]
async fn managed_workspace_binding_is_shared_and_ignores_stale_client_path() {
    let temp = tempfile::tempdir().unwrap();
    let (svc, _, repo, _) = make_service_with_workspace_root(temp.path().to_path_buf());
    let projects = make_injected_project_service(temp.path()).await;
    svc.with_project_service(projects.clone());
    let root = tempfile::tempdir().unwrap();
    let workspace = projects
        .create_workspace(
            "user_1",
            CreateWorkspaceRequest {
                name: "Shared".into(),
                uri: Some(to_file_uri(root.path()).unwrap()),
            },
        )
        .await
        .unwrap();
    let first = svc.create("user_1", managed_request(&workspace.id)).await.unwrap();
    let second = svc.create("user_1", managed_request(&workspace.id)).await.unwrap();
    for id in [&first.id, &second.id] {
        let row = repo.get("user_1", id).await.unwrap().unwrap();
        assert_eq!(row.project_id.as_deref(), Some(workspace.id.as_str()));
        assert_eq!(
            std::fs::canonicalize(
                svc.build_task_options(&row)
                    .await
                    .unwrap()
                    .context
                    .workspace
                    .stored_path
            )
            .unwrap(),
            root.path().canonicalize().unwrap()
        );
    }
    projects
        .remove_attached("user_1", &workspace.explorer.entries[0].pe_id)
        .await
        .unwrap();
    let row = repo.get("user_1", &first.id).await.unwrap().unwrap();
    let state = svc.conversation_workspace(&row).await.unwrap().unwrap();
    assert!(state.roots.is_empty());
    assert!(state.default_pe_id.is_none());
    // Chat can still initialize after its original root is removed.
    assert!(svc.build_task_options(&row).await.is_ok());
}

#[tokio::test]
async fn managed_workspace_binding_helper_inherits_empty_workspace_identity() {
    let temp = tempfile::tempdir().unwrap();
    let (svc, _, repo, _) = make_service_with_workspace_root(temp.path().to_path_buf());
    let projects = make_injected_project_service(temp.path()).await;
    svc.with_project_service(projects.clone());
    let workspace = projects
        .create_workspace(
            "user_1",
            CreateWorkspaceRequest {
                name: "Empty".into(),
                uri: None,
            },
        )
        .await
        .unwrap();
    let parent = svc.create("user_1", managed_request(&workspace.id)).await.unwrap();
    let child = svc
        .create_for_conversation_helper(
            "user_1",
            &parent.id,
            &aionui_api_types::ConversationCreateRequest {
                name: "Follow-up".into(),
                workspace: None,
                assistant_id: None,
            },
        )
        .await
        .unwrap();
    let row = repo.get("user_1", &child.id).await.unwrap().unwrap();
    assert_eq!(row.project_id.as_deref(), Some(workspace.id.as_str()));
}

#[tokio::test]
async fn managed_workspace_binding_rejects_other_owner_and_unknown_id() {
    let temp = tempfile::tempdir().unwrap();
    let (svc, _, repo, _) = make_service_with_workspace_root(temp.path().to_path_buf());
    let projects = make_injected_project_service(temp.path()).await;
    svc.with_project_service(projects.clone());
    let workspace = projects
        .create_workspace(
            "user_1",
            CreateWorkspaceRequest {
                name: "Empty".into(),
                uri: None,
            },
        )
        .await
        .unwrap();
    assert!(svc.create("user_2", managed_request(&workspace.id)).await.is_err());
    assert!(svc.create("user_1", managed_request("missing")).await.is_err());
    assert!(repo.rows.lock().unwrap().is_empty());
}

#[tokio::test]
async fn managed_workspace_binding_uses_current_default_instead_of_chat_creation_path() {
    let temp = tempfile::tempdir().unwrap();
    let (svc, _, repo, _) = make_service_with_workspace_root(temp.path().to_path_buf());
    let projects = make_injected_project_service(temp.path()).await;
    svc.with_project_service(projects.clone());
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let workspace = projects
        .create_workspace(
            "user_1",
            CreateWorkspaceRequest {
                name: "Change default".into(),
                uri: Some(to_file_uri(first.path()).unwrap()),
            },
        )
        .await
        .unwrap();
    let chat = svc.create("user_1", managed_request(&workspace.id)).await.unwrap();
    let updated = projects
        .update_workspace(
            "user_1",
            &workspace.id,
            UpdateWorkspaceRequest {
                name: workspace.name.clone(),
                default_pe_id: None,
                roots: vec![WorkspaceRootRequest {
                    pe_id: None,
                    uri: to_file_uri(second.path()).unwrap(),
                    display_name: None,
                }],
            },
        )
        .await
        .unwrap();
    let row = repo.get("user_1", &chat.id).await.unwrap().unwrap();
    let bootstrap = svc.managed_workspace_bootstrap(&row).await.unwrap().unwrap();
    assert_ne!(
        Path::new(&bootstrap).canonicalize().unwrap(),
        second.path().canonicalize().unwrap()
    );
    projects
        .update_workspace(
            "user_1",
            &workspace.id,
            UpdateWorkspaceRequest {
                name: updated.name,
                default_pe_id: Some(updated.explorer.entries[0].pe_id.clone()),
                roots: vec![WorkspaceRootRequest {
                    pe_id: Some(updated.explorer.entries[0].pe_id.clone()),
                    uri: to_file_uri(second.path()).unwrap(),
                    display_name: None,
                }],
            },
        )
        .await
        .unwrap();
    let options = svc.build_task_options(&row).await.unwrap();
    assert_eq!(
        Path::new(&options.context.workspace.path)
            .canonicalize()
            .unwrap(),
        second.path().canonicalize().unwrap()
    );
}

#[tokio::test]
async fn managed_workspace_binding_empty_and_missing_default_can_initialize() {
    let temp = tempfile::tempdir().unwrap();
    let (svc, _, repo, _) = make_service_with_workspace_root(temp.path().to_path_buf());
    let projects = make_injected_project_service(temp.path()).await;
    svc.with_project_service(projects.clone());
    let root = tempfile::tempdir().unwrap();
    let workspace = projects
        .create_workspace(
            "user_1",
            CreateWorkspaceRequest {
                name: "Missing".into(),
                uri: Some(to_file_uri(root.path()).unwrap()),
            },
        )
        .await
        .unwrap();
    root.close().unwrap();
    let conv = svc.create("user_1", managed_request(&workspace.id)).await.unwrap();
    let row = repo.get("user_1", &conv.id).await.unwrap().unwrap();
    assert!(svc.build_task_options(&row).await.is_ok());
    let state = svc.conversation_workspace(&row).await.unwrap().unwrap();
    assert_eq!(state.roots[0].runtime_status, aionui_project::RuntimeStatus::Missing);
    assert!(state.default_pe_id.is_some());
    projects
        .update_workspace(
            "user_1",
            &workspace.id,
            UpdateWorkspaceRequest {
                name: "Empty".into(),
                roots: vec![],
                default_pe_id: None,
            },
        )
        .await
        .unwrap();
    assert!(svc.build_task_options(&row).await.is_ok());
}

#[tokio::test]
async fn managed_workspace_binding_lease_lasts_until_cancelled_task_finishes() {
    let temp = tempfile::tempdir().unwrap();
    let (svc, _, _, _) = make_service_with_workspace_root(temp.path().to_path_buf());
    let projects = make_injected_project_service(temp.path()).await;
    svc.with_project_service(projects.clone());
    let root = tempfile::tempdir().unwrap();
    let workspace = projects
        .create_workspace(
            "user_1",
            CreateWorkspaceRequest {
                name: "Busy".into(),
                uri: Some(to_file_uri(root.path()).unwrap()),
            },
        )
        .await
        .unwrap();
    let conv = svc.create("user_1", managed_request(&workspace.id)).await.unwrap();
    let peer = svc.create("user_1", managed_request(&workspace.id)).await.unwrap();
    assert_ne!(conv.id, peer.id);
    let task_mgr = Arc::new(MockTaskManager::new());
    let agent = Arc::new(BlockingCancelAgent::new_with_type(&conv.id, AgentType::Aionrs));
    task_mgr.insert_agent(&conv.id, AgentInstance::Mock(agent.clone()));
    let task_dyn: Arc<dyn IWorkerTaskManager> = task_mgr;
    let started = agent.send_started.notified();
    tokio::pin!(started);
    // Register before spawning so Notify's wake cannot race the test.
    started.as_mut().enable();
    let send = svc
        .send_message("user_1", &conv.id, make_send_req(), &task_dyn)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), &mut started)
        .await
        .unwrap();
    let pe_id = &workspace.explorer.entries[0].pe_id;
    assert_eq!(
        projects.remove_attached("user_1", pe_id).await.unwrap_err().code(),
        "project_workspace_busy"
    );
    svc.cancel("user_1", &conv.id, &send.turn_id, &task_dyn).await.unwrap();
    assert_eq!(
        projects.remove_attached("user_1", pe_id).await.unwrap_err().code(),
        "project_workspace_busy"
    );
    agent.release_finish();
    wait_for_turn_released(&svc, &conv.id).await;
    projects.remove_attached("user_1", pe_id).await.unwrap();
}
