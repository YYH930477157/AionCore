use aionui_common::{generate_prefixed_id, now_ms};
use sqlx::{Row, SqlitePool};

use crate::error::DbError;
use crate::models::{FolderRow, ProjectExplorerRow, ProjectKind, ProjectRow};
use crate::repository::project::IProjectStore;

/// SQLite-backed implementation of [`IProjectStore`].
#[derive(Clone, Debug)]
pub struct SqliteProjectStore {
    pool: SqlitePool,
}

impl SqliteProjectStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

const FOLDER_COLS: &str = "folder_id, resource_uri, resource_canonical, created_at, updated_at";
const PROJECT_COLS: &str = "project_id, name, kind, created_at, updated_at";
const ENTRY_COLS: &str = "pe_id, project_id, folder_id, role, display_name, order_index, created_at, updated_at";

#[async_trait::async_trait]
impl IProjectStore for SqliteProjectStore {
    async fn get_workspace(&self, user_id: &str, project_id: &str) -> Result<Option<crate::WorkspaceRow>, DbError> {
        Ok(sqlx::query_as("SELECT w.project_id, w.config_path, w.dirty, w.default_pe_id, w.saved_json FROM project_workspaces w JOIN projects p ON p.project_id = w.project_id WHERE w.project_id = ? AND w.owner_user_id = ? AND p.user_id = ?")
            .bind(project_id).bind(user_id).bind(user_id).fetch_optional(&self.pool).await?)
    }

    async fn put_workspace(
        &self,
        user_id: &str,
        name: &str,
        workspace: &crate::WorkspaceRow,
        roots: &[crate::WorkspaceEntryInput],
    ) -> Result<(), DbError> {
        let now = now_ms();
        let project_id = &workspace.project_id;
        let mut tx = self.pool.begin().await?;
        let owner: Option<String> = sqlx::query_scalar("SELECT user_id FROM projects WHERE project_id = ?")
            .bind(project_id)
            .fetch_optional(&mut *tx)
            .await?;
        if owner.as_deref().is_some_and(|owner| owner != user_id) {
            return Err(DbError::NotFound("project".into()));
        }
        for root in roots {
            let existing: Option<(String, String)> =
                sqlx::query_as("SELECT project_id, owner_user_id FROM project_explorer WHERE pe_id = ?")
                    .bind(&root.pe_id)
                    .fetch_optional(&mut *tx)
                    .await?;
            if existing.is_some_and(|(project, owner)| project != *project_id || owner != user_id) {
                return Err(DbError::Conflict(
                    "workspace root identity belongs to another project".into(),
                ));
            }
        }
        sqlx::query("INSERT INTO projects (project_id, user_id, name, kind, created_at, updated_at) VALUES (?, ?, ?, 'standard', ?, ?) ON CONFLICT(project_id) DO UPDATE SET name = excluded.name, updated_at = excluded.updated_at")
            .bind(project_id).bind(user_id).bind(name).bind(now).bind(now).execute(&mut *tx).await?;
        // Delete/reinsert inside the transaction supports swaps of folder identities
        // without violating the project/folder unique index halfway through.
        sqlx::query("DELETE FROM project_explorer WHERE project_id = ? AND owner_user_id = ?")
            .bind(project_id)
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        for (position, root) in roots.iter().enumerate() {
            sqlx::query("INSERT INTO project_explorer (pe_id, project_id, owner_user_id, folder_id, role, display_name, order_index, created_at, updated_at) VALUES (?, ?, ?, ?, 'attached', ?, ?, ?, ?)")
                .bind(&root.pe_id).bind(project_id).bind(user_id).bind(&root.folder_id).bind(&root.display_name)
                .bind(position as i64).bind(now).bind(now).execute(&mut *tx).await?;
        }
        sqlx::query("INSERT INTO project_workspaces (project_id, owner_user_id, config_path, dirty, default_pe_id, saved_json) VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT(project_id) DO UPDATE SET config_path = excluded.config_path, dirty = excluded.dirty, default_pe_id = excluded.default_pe_id, saved_json = excluded.saved_json")
            .bind(project_id).bind(user_id).bind(&workspace.config_path).bind(workspace.dirty)
            .bind(&workspace.default_pe_id).bind(&workspace.saved_json).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }
    async fn upsert_folder(&self, canonical: &str, raw_uri: &str) -> Result<FolderRow, DbError> {
        let now = now_ms();
        let folder_id = generate_prefixed_id("folder");
        // INSERT OR IGNORE: a canonical collision leaves the existing row
        // untouched (immutable folder rows, no updated_at bump).
        sqlx::query(
            "INSERT OR IGNORE INTO folders (folder_id, resource_uri, resource_canonical, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&folder_id)
        .bind(raw_uri)
        .bind(canonical)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;

        let row = sqlx::query_as::<_, FolderRow>(&format!(
            "SELECT {FOLDER_COLS} FROM folders WHERE resource_canonical = ?"
        ))
        .bind(canonical)
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    async fn get_folder(&self, folder_id: &str) -> Result<Option<FolderRow>, DbError> {
        let row = sqlx::query_as::<_, FolderRow>(&format!("SELECT {FOLDER_COLS} FROM folders WHERE folder_id = ?"))
            .bind(folder_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    async fn get_project(&self, user_id: &str, project_id: &str) -> Result<Option<ProjectRow>, DbError> {
        let row = sqlx::query_as::<_, ProjectRow>(&format!(
            "SELECT {PROJECT_COLS} FROM projects WHERE project_id = ? AND user_id = ?"
        ))
        .bind(project_id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn select_workspace_entry_by_folder(
        &self,
        user_id: &str,
        folder_id: &str,
    ) -> Result<Option<ProjectExplorerRow>, DbError> {
        let row = sqlx::query_as::<_, ProjectExplorerRow>(&format!(
            "SELECT {ENTRY_COLS} FROM project_explorer \
             WHERE folder_id = ? AND role = 'workspace' AND owner_user_id = ?"
        ))
        .bind(folder_id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn get_entry(&self, user_id: &str, pe_id: &str) -> Result<Option<ProjectExplorerRow>, DbError> {
        let row = sqlx::query_as::<_, ProjectExplorerRow>(&format!(
            "SELECT {ENTRY_COLS} FROM project_explorer WHERE pe_id = ? AND owner_user_id = ?"
        ))
        .bind(pe_id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn list_entries(
        &self,
        user_id: &str,
        project_id: &str,
    ) -> Result<Vec<(ProjectExplorerRow, FolderRow)>, DbError> {
        // Manual join projection: project_explorer and folders share column
        // names (folder_id / created_at / updated_at), so folder columns are
        // aliased and rows are mapped by name rather than via a tuple FromRow.
        let rows = sqlx::query(
            "SELECT pe.pe_id, pe.project_id, pe.folder_id, pe.role, pe.display_name, pe.order_index, \
                    pe.created_at, pe.updated_at, \
                    f.resource_uri AS f_resource_uri, f.resource_canonical AS f_resource_canonical, \
                    f.created_at AS f_created_at, f.updated_at AS f_updated_at \
             FROM project_explorer pe \
             JOIN folders f ON f.folder_id = pe.folder_id \
             WHERE pe.project_id = ? AND pe.owner_user_id = ? \
             ORDER BY pe.order_index ASC, pe.created_at ASC",
        )
        .bind(project_id)
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;

        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let folder_id: String = r.get("folder_id");
            let entry = ProjectExplorerRow {
                pe_id: r.get("pe_id"),
                project_id: r.get("project_id"),
                folder_id: folder_id.clone(),
                role: r.get("role"),
                display_name: r.get("display_name"),
                order_index: r.get("order_index"),
                created_at: r.get("created_at"),
                updated_at: r.get("updated_at"),
            };
            let folder = FolderRow {
                folder_id,
                resource_uri: r.get("f_resource_uri"),
                resource_canonical: r.get("f_resource_canonical"),
                created_at: r.get("f_created_at"),
                updated_at: r.get("f_updated_at"),
            };
            out.push((entry, folder));
        }
        Ok(out)
    }

    async fn create_project_with_workspace_entry(
        &self,
        user_id: &str,
        folder_id: &str,
        name: &str,
        kind: ProjectKind,
    ) -> Result<(ProjectRow, ProjectExplorerRow), DbError> {
        let now = now_ms();
        let project_id = generate_prefixed_id("project");
        let pe_id = generate_prefixed_id("pe");

        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO projects (project_id, user_id, name, kind, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(&project_id)
        .bind(user_id)
        .bind(name)
        .bind(kind.as_str())
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;

        let entry_insert = sqlx::query(
            "INSERT INTO project_explorer \
                 (pe_id, project_id, owner_user_id, folder_id, role, display_name, order_index, created_at, updated_at) \
             VALUES (?, ?, ?, ?, 'workspace', NULL, 0, ?, ?)",
        )
        .bind(&pe_id)
        .bind(&project_id)
        .bind(user_id)
        .bind(folder_id)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await;

        match entry_insert {
            Ok(_) => {
                tx.commit().await?;
                let project = self
                    .get_project(user_id, &project_id)
                    .await?
                    .ok_or_else(|| DbError::NotFound(format!("project {project_id} after insert")))?;
                let entry = self
                    .get_entry(user_id, &pe_id)
                    .await?
                    .ok_or_else(|| DbError::NotFound(format!("project_explorer {pe_id} after insert")))?;
                Ok((project, entry))
            }
            Err(err) => {
                tx.rollback().await?;
                let db_err = DbError::from(err);
                if db_err.is_unique_violation() {
                    // This owner already has a workspace project for this folder — return it.
                    let entry = self
                        .select_workspace_entry_by_folder(user_id, folder_id)
                        .await?
                        .ok_or_else(|| DbError::Conflict(format!("workspace entry for folder {folder_id}")))?;
                    let project = self
                        .get_project(user_id, &entry.project_id)
                        .await?
                        .ok_or_else(|| DbError::NotFound(format!("project {}", entry.project_id)))?;
                    Ok((project, entry))
                } else {
                    Err(db_err)
                }
            }
        }
    }

    async fn insert_attached_entry(
        &self,
        user_id: &str,
        project_id: &str,
        folder_id: &str,
        display_name: Option<&str>,
        order_index: i64,
    ) -> Result<ProjectExplorerRow, DbError> {
        let now = now_ms();
        let pe_id = generate_prefixed_id("pe");
        sqlx::query(
            "INSERT INTO project_explorer \
                 (pe_id, project_id, owner_user_id, folder_id, role, display_name, order_index, created_at, updated_at) \
             VALUES (?, ?, ?, ?, 'attached', ?, ?, ?, ?)",
        )
        .bind(&pe_id)
        .bind(project_id)
        .bind(user_id)
        .bind(folder_id)
        .bind(display_name)
        .bind(order_index)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        self.get_entry(user_id, &pe_id)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("project_explorer {pe_id} after insert")))
    }

    async fn remove_entry(&self, user_id: &str, pe_id: &str) -> Result<(), DbError> {
        sqlx::query("DELETE FROM project_explorer WHERE pe_id = ? AND owner_user_id = ?")
            .bind(pe_id)
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn delete_project(&self, user_id: &str, project_id: &str) -> Result<(), DbError> {
        // Explorer entries first, then the project row — both owner-scoped, one
        // transaction. `folders` are global and intentionally left intact.
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM project_workspaces WHERE project_id = ? AND owner_user_id = ?")
            .bind(project_id)
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM project_explorer WHERE project_id = ? AND owner_user_id = ?")
            .bind(project_id)
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM projects WHERE project_id = ? AND user_id = ?")
            .bind(project_id)
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn reorder(&self, user_id: &str, project_id: &str, ordered_pe_ids: &[String]) -> Result<(), DbError> {
        let now = now_ms();
        let mut tx = self.pool.begin().await?;
        for (position, pe_id) in ordered_pe_ids.iter().enumerate() {
            sqlx::query(
                "UPDATE project_explorer SET order_index = ?, updated_at = ? \
                 WHERE pe_id = ? AND project_id = ? AND owner_user_id = ?",
            )
            .bind(position as i64)
            .bind(now)
            .bind(pe_id)
            .bind(project_id)
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn rename_entry(
        &self,
        user_id: &str,
        pe_id: &str,
        display_name: Option<&str>,
    ) -> Result<ProjectExplorerRow, DbError> {
        let now = now_ms();
        let result = sqlx::query(
            "UPDATE project_explorer SET display_name = ?, updated_at = ? WHERE pe_id = ? AND owner_user_id = ?",
        )
        .bind(display_name)
        .bind(now)
        .bind(pe_id)
        .bind(user_id)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("project_explorer {pe_id}")));
        }
        self.get_entry(user_id, pe_id)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("project_explorer {pe_id}")))
    }
}
