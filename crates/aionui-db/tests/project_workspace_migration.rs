//! Migration coverage for the managed-workspace columns.
//!
//! `projects` gained `workspace_managed` / `workspace_config_path` /
//! `workspace_default_pe_id` / `workspace_dirty` in 045. Every existing project row must come out
//! of the upgrade as a legacy single-root project, so the defaults matter as much as the columns.

use aionui_db::{IProjectStore, ProjectKind, SqliteProjectStore, init_database_memory};

/// `(not_null, default)` for a `projects` column.
async fn column_shape(db: &aionui_db::Database, name: &str) -> (i64, Option<String>) {
    let rows: Vec<(String, i64, Option<String>)> =
        sqlx::query_as("SELECT name, \"notnull\", dflt_value FROM pragma_table_info('projects')")
            .fetch_all(db.pool())
            .await
            .unwrap();
    let (_, not_null, default) = rows
        .iter()
        .find(|(column, _, _)| column == name)
        .unwrap_or_else(|| panic!("missing column {name}"));
    (*not_null, default.clone())
}

#[tokio::test]
async fn migration_adds_workspace_columns_with_legacy_defaults() {
    let db = init_database_memory().await.unwrap();

    let (managed_not_null, managed_default) = column_shape(&db, "workspace_managed").await;
    assert_eq!(managed_not_null, 1);
    assert_eq!(managed_default.as_deref(), Some("0"));

    let (dirty_not_null, dirty_default) = column_shape(&db, "workspace_dirty").await;
    assert_eq!(dirty_not_null, 1);
    assert_eq!(dirty_default.as_deref(), Some("0"));

    let (config_not_null, config_default) = column_shape(&db, "workspace_config_path").await;
    assert_eq!(config_not_null, 0);
    assert_eq!(config_default, None);

    let (default_not_null, default_default) = column_shape(&db, "workspace_default_pe_id").await;
    assert_eq!(default_not_null, 0);
    assert_eq!(default_default, None);
}

#[tokio::test]
async fn existing_project_rows_read_back_as_legacy() {
    let db = init_database_memory().await.unwrap();
    let store = SqliteProjectStore::new(db.pool().clone());
    let folder = store
        .upsert_folder("file:///tmp/legacy", "file:///tmp/legacy")
        .await
        .unwrap();
    let (project, _entry) = store
        .create_project_with_workspace_entry(
            "system_default_user",
            &folder.folder_id,
            "legacy",
            ProjectKind::Standard,
        )
        .await
        .unwrap();

    assert_eq!(project.workspace_managed, 0);
    assert_eq!(project.workspace_dirty, 0);
    assert!(project.workspace_config_path.is_none());
    assert!(project.workspace_default_pe_id.is_none());
}
