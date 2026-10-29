use aionui_api_types::{CreateWorkspaceRequest, UpdateWorkspaceRequest, WorkspaceRootRequest};
use aionui_db::{SqliteProjectStore, init_database_memory};
use aionui_project::{ProjectService, canonical::to_file_uri};
use std::sync::Arc;

const USER: &str = "system_default_user";
async fn service() -> (ProjectService, aionui_db::Database) {
    let db = init_database_memory().await.unwrap();
    let svc = ProjectService::new(
        Arc::new(SqliteProjectStore::new(db.pool().clone())),
        std::env::temp_dir(),
    );
    (svc, db)
}

#[tokio::test]
async fn workspace_identity_is_independent_and_last_root_is_removable() {
    let (svc, _db) = service().await;
    let dir = tempfile::tempdir().unwrap();
    let req = CreateWorkspaceRequest {
        name: "Research".into(),
        uri: Some(to_file_uri(dir.path()).unwrap()),
    };
    let a = svc.create_workspace(USER, req.clone()).await.unwrap();
    let b = svc.create_workspace(USER, req).await.unwrap();
    let legacy = svc
        .create_standard(USER, to_file_uri(dir.path()).unwrap())
        .await
        .unwrap();
    assert_ne!(a.id, b.id);
    assert_ne!(a.id, legacy.project.project_id);
    assert_eq!(a.explorer.entries[0].folder_id, legacy.folder.folder_id);
    assert_eq!(a.explorer.entries[0].role, "attached");
    svc.remove_attached(USER, &a.explorer.entries[0].pe_id).await.unwrap();
    let empty = svc.get_project(USER, &a.id).await.unwrap();
    assert!(empty.explorer.entries.is_empty());
    assert_eq!(empty.workspace.unwrap().default_pe_id, None);
}

#[tokio::test]
async fn saved_snapshot_restores_ids_order_aliases_and_never_changes_root_files() {
    let (svc, _db) = service().await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("source");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("keep.txt"), "keep").unwrap();
    let a = svc
        .create_workspace(
            USER,
            CreateWorkspaceRequest {
                name: "Original".into(),
                uri: Some(to_file_uri(&root).unwrap()),
            },
        )
        .await
        .unwrap();
    let pe = a.explorer.entries[0].pe_id.clone();
    let file = dir.path().join("sample.woody-workspace");
    svc.save_workspace(USER, &a.id, Some(file.to_string_lossy().into_owned()))
        .await
        .unwrap();
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    assert_eq!(saved["roots"][0]["path"], "source");
    svc.update_workspace(
        USER,
        &a.id,
        UpdateWorkspaceRequest {
            name: "Edited".into(),
            roots: vec![],
            default_pe_id: None,
        },
    )
    .await
    .unwrap();
    assert!(svc.get_project(USER, &a.id).await.unwrap().workspace.unwrap().dirty);
    let restored = svc.discard_workspace(USER, &a.id).await.unwrap();
    assert_eq!(restored.name, "Original");
    assert_eq!(restored.explorer.entries[0].pe_id, pe);
    assert!(!restored.workspace.unwrap().dirty);
    assert_eq!(std::fs::read_to_string(root.join("keep.txt")).unwrap(), "keep");
    let reopened = svc
        .open_workspace(USER, file.to_string_lossy().into_owned())
        .await
        .unwrap();
    assert_eq!(reopened.id, a.id);
}

#[tokio::test]
async fn active_task_blocks_removal_replacement_and_discard() {
    let (svc, _db) = service().await;
    let dir = tempfile::tempdir().unwrap();
    let a = svc
        .create_workspace(
            USER,
            CreateWorkspaceRequest {
                name: "Busy".into(),
                uri: Some(to_file_uri(dir.path()).unwrap()),
            },
        )
        .await
        .unwrap();
    let guard = svc.acquire_workspace_task(USER, &a.id).await.unwrap();
    assert_eq!(
        svc.remove_attached(USER, &a.explorer.entries[0].pe_id)
            .await
            .unwrap_err()
            .code(),
        "project_workspace_busy"
    );
    assert_eq!(
        svc.update_workspace(
            USER,
            &a.id,
            UpdateWorkspaceRequest {
                name: "Busy".into(),
                roots: vec![],
                default_pe_id: None
            }
        )
        .await
        .unwrap_err()
        .code(),
        "project_workspace_busy"
    );
    assert_eq!(
        svc.discard_workspace(USER, &a.id).await.unwrap_err().code(),
        "project_workspace_busy"
    );
    drop(guard);
    svc.remove_attached(USER, &a.explorer.entries[0].pe_id).await.unwrap();
}

#[tokio::test]
async fn missing_roots_are_preserved_and_overlaps_and_foreign_access_are_rejected() {
    let (svc, _db) = service().await;
    let dir = tempfile::tempdir().unwrap();
    let a = svc
        .create_workspace(
            USER,
            CreateWorkspaceRequest {
                name: "Draft".into(),
                uri: None,
            },
        )
        .await
        .unwrap();
    let missing = WorkspaceRootRequest {
        pe_id: None,
        uri: to_file_uri(&dir.path().join("missing")).unwrap(),
        display_name: Some("Offline".into()),
    };
    let detail = svc
        .update_workspace(
            USER,
            &a.id,
            UpdateWorkspaceRequest {
                name: "Draft".into(),
                roots: vec![missing.clone()],
                default_pe_id: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(detail.explorer.entries[0].folder.runtime_status.as_str(), "missing");
    assert_eq!(detail.workspace.unwrap().default_pe_id, None);
    let ancestor = WorkspaceRootRequest {
        pe_id: None,
        uri: to_file_uri(dir.path()).unwrap(),
        display_name: None,
    };
    assert_eq!(
        svc.update_workspace(
            USER,
            &a.id,
            UpdateWorkspaceRequest {
                name: "Draft".into(),
                roots: vec![missing, ancestor],
                default_pe_id: None
            }
        )
        .await
        .unwrap_err()
        .code(),
        "project_explorer_overlap"
    );
    assert_eq!(
        svc.acquire_workspace_task("other-user", &a.id)
            .await
            .unwrap_err()
            .code(),
        "project_not_found"
    );
}

#[tokio::test]
async fn legacy_promotion_and_relocation_preserve_project_and_root_identity() {
    let (svc, _db) = service().await;
    let dir = tempfile::tempdir().unwrap();
    let first = dir.path().join("first");
    let second = dir.path().join("second");
    std::fs::create_dir(&first).unwrap();
    let old = svc.create_standard(USER, to_file_uri(&first).unwrap()).await.unwrap();
    let pe = old.project_explorer.pe_id;
    let project = old.project.project_id;
    let result = svc
        .update_workspace(
            USER,
            &project,
            UpdateWorkspaceRequest {
                name: "Promoted".into(),
                roots: vec![WorkspaceRootRequest {
                    pe_id: Some(pe.clone()),
                    uri: to_file_uri(&second).unwrap(),
                    display_name: Some("Relocated".into()),
                }],
                default_pe_id: Some(pe.clone()),
            },
        )
        .await
        .unwrap();
    assert_eq!(result.id, project);
    assert_eq!(result.explorer.entries[0].pe_id, pe);
    assert_eq!(result.explorer.entries[0].display_name.as_deref(), Some("Relocated"));
    assert_eq!(result.workspace.unwrap().default_pe_id, Some(pe));
}

#[tokio::test]
async fn invalid_default_and_root_identity_leave_draft_unchanged() {
    let (svc, _db) = service().await;
    let a = svc
        .create_workspace(
            USER,
            CreateWorkspaceRequest {
                name: "Draft".into(),
                uri: None,
            },
        )
        .await
        .unwrap();
    let err = svc
        .update_workspace(
            USER,
            &a.id,
            UpdateWorkspaceRequest {
                name: "Invalid".into(),
                roots: vec![],
                default_pe_id: Some("absent".into()),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), "invalid_workspace");
    assert_eq!(svc.get_project(USER, &a.id).await.unwrap().name, "Draft");
}

#[tokio::test]
async fn moved_config_resolves_relative_roots_and_preserves_external_roots() {
    let (svc, _db) = service().await;
    let dir = tempfile::tempdir().unwrap();
    let original = dir.path().join("original");
    let moved = dir.path().join("moved");
    let external = dir.path().join("external");
    std::fs::create_dir_all(original.join("code")).unwrap();
    std::fs::create_dir(&external).unwrap();
    let a = svc
        .create_workspace(
            USER,
            CreateWorkspaceRequest {
                name: "Portable".into(),
                uri: Some(to_file_uri(&original.join("code")).unwrap()),
            },
        )
        .await
        .unwrap();
    let pe = a.explorer.entries[0].pe_id.clone();
    svc.attach_folder(
        USER,
        aionui_project::AttachInput {
            project_id: a.id.clone(),
            uri: to_file_uri(&external).unwrap(),
            display_name: Some("Shared".into()),
        },
    )
    .await
    .unwrap();
    svc.save_workspace(
        USER,
        &a.id,
        Some(original.join("test.woody-workspace").to_string_lossy().into_owned()),
    )
    .await
    .unwrap();
    std::fs::rename(&original, &moved).unwrap();
    let reopened = svc
        .open_workspace(USER, moved.join("test.woody-workspace").to_string_lossy().into_owned())
        .await
        .unwrap();
    assert_eq!(reopened.id, a.id);
    assert_eq!(reopened.explorer.entries[0].pe_id, pe);
    assert_eq!(reopened.explorer.entries[0].folder.runtime_status.as_str(), "available");
    assert_eq!(reopened.explorer.entries[1].folder.runtime_status.as_str(), "available");
    assert_eq!(reopened.explorer.entries[1].display_name.as_deref(), Some("Shared"));
    assert_eq!(
        aionui_project::canonical::uri_to_path(&reopened.explorer.entries[0].folder.resource_uri).unwrap(),
        moved.join("code")
    );
}

#[tokio::test]
async fn restart_preserves_unnamed_draft_and_explicit_no_default() {
    let (svc, db) = service().await;
    let a = svc
        .create_workspace(
            USER,
            CreateWorkspaceRequest {
                name: "Unsaved".into(),
                uri: None,
            },
        )
        .await
        .unwrap();
    drop(svc);
    let restarted = ProjectService::new(
        Arc::new(SqliteProjectStore::new(db.pool().clone())),
        std::env::temp_dir(),
    );
    let detail = restarted.get_project(USER, &a.id).await.unwrap();
    assert_eq!(detail.name, "Unsaved");
    assert!(detail.explorer.entries.is_empty());
    let state = detail.workspace.unwrap();
    assert!(state.dirty);
    assert!(state.config_path.is_none());
    assert!(state.default_pe_id.is_none());
}

#[tokio::test]
async fn managed_binding_uses_explicit_default_and_never_first_root_fallback() {
    let (svc, _db) = service().await;
    let dir = tempfile::tempdir().unwrap();
    let a = svc
        .create_workspace(
            USER,
            CreateWorkspaceRequest {
                name: "Bound".into(),
                uri: Some(to_file_uri(dir.path()).unwrap()),
            },
        )
        .await
        .unwrap();
    let entry = &a.explorer.entries[0];
    svc.validate_workspace_match(USER, &a.id, &entry.folder_id)
        .await
        .unwrap();
    svc.update_workspace(
        USER,
        &a.id,
        UpdateWorkspaceRequest {
            name: a.name,
            roots: vec![WorkspaceRootRequest {
                pe_id: Some(entry.pe_id.clone()),
                uri: entry.folder.resource_uri.clone(),
                display_name: None,
            }],
            default_pe_id: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        svc.validate_workspace_match(USER, &a.id, &entry.folder_id)
            .await
            .unwrap_err()
            .code(),
        "workspace_folder_mismatch"
    );
}

#[tokio::test]
async fn unnamed_discard_keeps_project_and_clears_draft_without_deleting_roots() {
    let (svc, _db) = service().await;
    let dir = tempfile::tempdir().unwrap();
    let a = svc
        .create_workspace(
            USER,
            CreateWorkspaceRequest {
                name: "Original draft".into(),
                uri: Some(to_file_uri(dir.path()).unwrap()),
            },
        )
        .await
        .unwrap();
    let restored = svc.discard_workspace(USER, &a.id).await.unwrap();
    assert_eq!(restored.id, a.id);
    assert_eq!(restored.name, "Original draft");
    assert!(restored.explorer.entries.is_empty());
    assert!(!restored.workspace.unwrap().dirty);
    assert!(dir.path().is_dir());
}

#[tokio::test]
async fn discard_first_legacy_edit_restores_original_roots_and_identity() {
    let (svc, _db) = service().await;
    let dir = tempfile::tempdir().unwrap();
    let old = svc
        .create_standard(USER, to_file_uri(dir.path()).unwrap())
        .await
        .unwrap();
    svc.update_workspace(
        USER,
        &old.project.project_id,
        UpdateWorkspaceRequest {
            name: "Changed".into(),
            roots: vec![],
            default_pe_id: None,
        },
    )
    .await
    .unwrap();
    let restored = svc.discard_workspace(USER, &old.project.project_id).await.unwrap();
    assert_eq!(restored.name, old.project.name);
    assert_eq!(restored.explorer.entries[0].pe_id, old.project_explorer.pe_id);
    assert_eq!(
        restored.workspace.unwrap().default_pe_id,
        Some(old.project_explorer.pe_id)
    );
}

#[tokio::test]
async fn save_is_allowed_during_active_task_without_changing_membership() {
    let (svc, _db) = service().await;
    let dir = tempfile::tempdir().unwrap();
    let a = svc
        .create_workspace(
            USER,
            CreateWorkspaceRequest {
                name: "Busy save".into(),
                uri: Some(to_file_uri(dir.path()).unwrap()),
            },
        )
        .await
        .unwrap();
    let _guard = svc.acquire_workspace_task(USER, &a.id).await.unwrap();
    let state = svc
        .save_workspace(
            USER,
            &a.id,
            Some(dir.path().join("active.woody-workspace").to_string_lossy().into_owned()),
        )
        .await
        .unwrap();
    assert!(!state.dirty);
    assert_eq!(
        svc.get_project(USER, &a.id).await.unwrap().explorer.entries[0].pe_id,
        a.explorer.entries[0].pe_id
    );
}

#[tokio::test]
async fn relocation_emits_invalidation_for_stable_old_root_identity() {
    let (svc, _db) = service().await;
    let dir = tempfile::tempdir().unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    svc.set_fs_roots_sender(tx);
    let a = svc
        .create_workspace(
            USER,
            CreateWorkspaceRequest {
                name: "Moved".into(),
                uri: Some(to_file_uri(&dir.path().join("old")).unwrap()),
            },
        )
        .await
        .unwrap();
    let pe = a.explorer.entries[0].pe_id.clone();
    svc.update_workspace(
        USER,
        &a.id,
        UpdateWorkspaceRequest {
            name: a.name,
            roots: vec![WorkspaceRootRequest {
                pe_id: Some(pe.clone()),
                uri: to_file_uri(&dir.path().join("new")).unwrap(),
                display_name: None,
            }],
            default_pe_id: Some(pe.clone()),
        },
    )
    .await
    .unwrap();
    assert!(
        matches!(rx.try_recv().unwrap(), aionui_project::monitor::FsInbound::InvalidateRoots { pe_ids } if pe_ids == vec![pe])
    );
}

#[tokio::test]
async fn open_workspace_rejects_legacy_identity_collision_without_promoting_it() {
    let (svc, _db) = service().await;
    let dir = tempfile::tempdir().unwrap();
    let old = svc
        .create_standard(USER, to_file_uri(dir.path()).unwrap())
        .await
        .unwrap();
    let path = dir.path().join("collision.woody-workspace");
    std::fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!({
            "version":1,"project_id":old.project.project_id,"name":"Collision",
            "default_pe_id":null,"roots":[]
        }))
        .unwrap(),
    )
    .unwrap();
    let err = svc
        .open_workspace(USER, path.to_string_lossy().into_owned())
        .await
        .unwrap_err();
    assert_eq!(err.code(), "invalid_workspace");
    let unchanged = svc.get_project(USER, &old.project.project_id).await.unwrap();
    assert_eq!(unchanged.name, old.project.name);
    assert!(unchanged.workspace.is_none());
    assert_eq!(unchanged.explorer.entries[0].pe_id, old.project_explorer.pe_id);
}
