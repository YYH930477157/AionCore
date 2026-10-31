//! Managed-workspace behaviour: creation, root editing, save/discard round-trips and the
//! refusals that keep a workspace coherent (duplicate / overlapping / unknown default roots).

use std::sync::Arc;

use aionui_db::{Database, IProjectStore, SqliteProjectStore, init_database_memory};
use tempfile::TempDir;

use super::{WorkspaceFile, WorkspaceFileRoot, WorkspaceRootInput, WorkspaceUpdateInput};
use crate::ProjectService;
use crate::canonical::to_file_uri;
use crate::types::ProjectError;

/// A service over a fresh in-memory DB; the tempdirs keep root folders alive.
async fn fixture() -> (ProjectService, Database) {
    let db = init_database_memory().await.unwrap();
    let store: Arc<dyn IProjectStore> = Arc::new(SqliteProjectStore::new(db.pool().clone()));
    let service = ProjectService::new(store, std::env::temp_dir());
    (service, db)
}

fn input(name: &str, roots: Vec<WorkspaceRootInput>, default_pe_id: Option<String>) -> WorkspaceUpdateInput {
    WorkspaceUpdateInput {
        name: name.to_owned(),
        roots,
        default_pe_id,
    }
}

fn root(uri: String) -> WorkspaceRootInput {
    WorkspaceRootInput {
        pe_id: None,
        uri,
        display_name: None,
    }
}

#[tokio::test]
async fn creates_an_empty_workspace_when_no_root_is_given() {
    let (service, _db) = fixture().await;
    let detail = service
        .create_managed("system_default_user", Some("Plan".to_owned()), None)
        .await
        .unwrap();

    assert_eq!(detail.name, "Plan");
    assert!(detail.explorer.entries.is_empty());
    let workspace = detail.workspace.expect("managed workspaces report their state");
    assert!(workspace.managed);
    assert!(workspace.config_path.is_none());
    assert!(workspace.default_pe_id.is_none());
    assert!(!workspace.dirty);
}

#[tokio::test]
async fn creates_a_workspace_rooted_at_the_first_folder() {
    let (service, _db) = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let detail = service
        .create_managed(
            "system_default_user",
            Some("Plan".to_owned()),
            Some(to_file_uri(dir.path()).unwrap()),
        )
        .await
        .unwrap();

    assert_eq!(detail.explorer.entries.len(), 1);
    assert_eq!(detail.explorer.entries[0].role, "attached");
    let workspace = detail.workspace.expect("managed");
    assert_eq!(
        workspace.default_pe_id.as_deref(),
        Some(detail.explorer.entries[0].pe_id.as_str())
    );
    assert!(workspace.config_path.is_none());
}

#[tokio::test]
async fn editing_roots_keeps_the_pe_id_of_a_surviving_root() {
    let (service, _db) = fixture().await;
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();

    let created = service
        .create_managed(
            "system_default_user",
            Some("Plan".to_owned()),
            Some(to_file_uri(first.path()).unwrap()),
        )
        .await
        .unwrap();
    let project_id = created.id.clone();
    let first_pe_id = created.explorer.entries[0].pe_id.clone();
    let first_uri = to_file_uri(first.path()).unwrap();

    // Append a second root, then drop it again: the first root keeps its identity throughout,
    // which is what keeps the frontend's tabs and fs subscriptions valid.
    let added = service
        .update_managed(
            "system_default_user",
            &project_id,
            input(
                "Plan",
                vec![root(first_uri.clone()), root(to_file_uri(second.path()).unwrap())],
                None,
            ),
        )
        .await
        .unwrap();
    assert_eq!(added.explorer.entries.len(), 2);
    assert_eq!(added.explorer.entries[0].pe_id, first_pe_id);
    assert_eq!(added.explorer.entries[1].order_index, 1);

    let removed = service
        .update_managed(
            "system_default_user",
            &project_id,
            input("Plan", vec![root(first_uri)], None),
        )
        .await
        .unwrap();
    assert_eq!(removed.explorer.entries.len(), 1);
    assert_eq!(removed.explorer.entries[0].pe_id, first_pe_id);
}

#[tokio::test]
async fn refuses_a_repeated_or_nested_root() {
    let (service, _db) = fixture().await;
    let outer = tempfile::tempdir().unwrap();
    let nested = outer.path().join("inner");
    std::fs::create_dir(&nested).unwrap();
    let duplicate = tempfile::tempdir().unwrap();

    let created = service
        .create_managed(
            "system_default_user",
            Some("Plan".to_owned()),
            Some(to_file_uri(outer.path()).unwrap()),
        )
        .await
        .unwrap();
    let project_id = created.id.clone();
    let outer_uri = to_file_uri(outer.path()).unwrap();

    let repeated = service
        .update_managed(
            "system_default_user",
            &project_id,
            input("Plan", vec![root(outer_uri.clone()), root(outer_uri.clone())], None),
        )
        .await;
    assert!(matches!(repeated, Err(ProjectError::ProjectExplorerDuplicate { .. })));

    let overlapping = service
        .update_managed(
            "system_default_user",
            &project_id,
            input("Plan", vec![root(outer_uri), root(to_file_uri(&nested).unwrap())], None),
        )
        .await;
    assert!(matches!(overlapping, Err(ProjectError::ProjectExplorerOverlap { .. })));

    // The rejected updates left the stored roots alone.
    let detail = service.get_project("system_default_user", &project_id).await.unwrap();
    assert_eq!(detail.explorer.entries.len(), 1);
    let _ = duplicate;
}

#[tokio::test]
async fn refuses_a_default_root_the_workspace_does_not_have() {
    let (service, _db) = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let created = service
        .create_managed(
            "system_default_user",
            Some("Plan".to_owned()),
            Some(to_file_uri(dir.path()).unwrap()),
        )
        .await
        .unwrap();

    let result = service
        .update_managed(
            "system_default_user",
            &created.id,
            input(
                "Plan",
                vec![root(to_file_uri(dir.path()).unwrap())],
                Some("pe_missing".to_owned()),
            ),
        )
        .await;
    assert!(matches!(result, Err(ProjectError::WorkspaceDefaultRootInvalid { .. })));
}

#[tokio::test]
async fn save_writes_the_file_and_discard_restores_it() {
    let (service, _db) = fixture().await;
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let config_path = config_dir.path().join("plan.woody-workspace");

    let created = service
        .create_managed(
            "system_default_user",
            Some("Plan".to_owned()),
            Some(to_file_uri(first.path()).unwrap()),
        )
        .await
        .unwrap();
    let project_id = created.id.clone();

    // Saving without a path and without a previous file cannot work: the caller must pick one.
    assert!(matches!(
        service.save_managed("system_default_user", &project_id, None).await,
        Err(ProjectError::WorkspacePathRequired)
    ));

    let saved = service
        .save_managed("system_default_user", &project_id, Some(&config_path))
        .await
        .unwrap();
    assert_eq!(
        saved.config_path.as_deref(),
        Some(config_path.to_string_lossy().as_ref())
    );
    assert!(!saved.dirty);
    let written: WorkspaceFile = serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
    assert_eq!(written.name, "Plan");
    assert_eq!(written.roots.len(), 1);
    assert_eq!(written.default_root, Some(0));

    // An edit after saving is "dirty" until it is saved again.
    let edited = service
        .update_managed(
            "system_default_user",
            &project_id,
            input(
                "Plan v2",
                vec![
                    root(to_file_uri(first.path()).unwrap()),
                    root(to_file_uri(second.path()).unwrap()),
                ],
                None,
            ),
        )
        .await
        .unwrap();
    assert_eq!(edited.name, "Plan v2");
    assert_eq!(edited.explorer.entries.len(), 2);
    assert!(edited.workspace.as_ref().unwrap().dirty);

    // Discard re-reads the file, so the project goes back to one root and its saved name.
    let discarded = service
        .discard_managed("system_default_user", &project_id)
        .await
        .unwrap();
    assert_eq!(discarded.name, "Plan");
    assert_eq!(discarded.explorer.entries.len(), 1);
    assert!(!discarded.workspace.as_ref().unwrap().dirty);
}

#[tokio::test]
async fn opens_a_workspace_file() {
    let (service, _db) = fixture().await;
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let config_path = config_dir.path().join("shared.woody-workspace");

    let file = WorkspaceFile {
        version: 1,
        name: "Shared".to_owned(),
        roots: vec![
            WorkspaceFileRoot {
                uri: to_file_uri(first.path()).unwrap(),
                display_name: Some("app".to_owned()),
            },
            WorkspaceFileRoot {
                uri: to_file_uri(second.path()).unwrap(),
                display_name: None,
            },
        ],
        default_root: Some(1),
    };
    std::fs::write(&config_path, serde_json::to_vec_pretty(&file).unwrap()).unwrap();

    let detail = service.open_managed("system_default_user", &config_path).await.unwrap();
    assert_eq!(detail.name, "Shared");
    assert_eq!(detail.explorer.entries.len(), 2);
    assert_eq!(detail.explorer.entries[0].display_name.as_deref(), Some("app"));
    let workspace = detail.workspace.unwrap();
    assert!(workspace.managed);
    assert!(!workspace.dirty);
    assert_eq!(
        workspace.default_pe_id.as_deref(),
        Some(detail.explorer.entries[1].pe_id.as_str())
    );
}

#[tokio::test]
async fn refuses_a_workspace_file_from_an_unknown_version() {
    let (service, _db) = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("future.woody-workspace");
    std::fs::write(&path, br#"{"version":99,"name":"Future","roots":[]}"#).unwrap();

    let result = service.open_managed("system_default_user", &path).await;
    assert!(matches!(result, Err(ProjectError::WorkspaceFileInvalid { .. })));
}

#[tokio::test]
async fn legacy_projects_stay_outside_the_managed_path() {
    let (service, _db) = fixture().await;
    let dir = TempDir::new().unwrap();
    let created = service
        .create_standard("system_default_user", to_file_uri(dir.path()).unwrap())
        .await
        .unwrap();
    let project_id = created.project.project_id;

    let detail = service.get_project("system_default_user", &project_id).await.unwrap();
    assert!(detail.workspace.is_none());

    let result = service
        .update_managed("system_default_user", &project_id, input("Nope", Vec::new(), None))
        .await;
    assert!(matches!(result, Err(ProjectError::WorkspaceNotManaged { .. })));
}
