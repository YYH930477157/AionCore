use aionui_common::{generate_prefixed_id, now_ms};
use sqlx::{Row, SqlitePool};

use crate::error::DbError;
use crate::models::{FolderRow, ProjectExplorerRow, ProjectKind, ProjectRow, Role};
use crate::repository::project::{IProjectStore, RootSpec};

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
const PROJECT_COLS: &str = "project_id, name, kind, created_at, updated_at, workspace_managed, \
     workspace_config_path, workspace_default_pe_id, workspace_dirty";
const ENTRY_COLS: &str = "pe_id, project_id, folder_id, role, display_name, order_index, created_at, updated_at";

#[async_trait::async_trait]
impl IProjectStore for SqliteProjectStore {
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

    async fn create_project(
        &self,
        user_id: &str,
        name: &str,
        kind: ProjectKind,
        managed: bool,
    ) -> Result<ProjectRow, DbError> {
        let now = now_ms();
        let project_id = generate_prefixed_id("proj");
        sqlx::query(
            "INSERT INTO projects (project_id, user_id, name, kind, created_at, updated_at, \
             workspace_managed, workspace_config_path, workspace_default_pe_id, workspace_dirty) \
             VALUES (?, ?, ?, ?, ?, ?, ?, NULL, NULL, 0)",
        )
        .bind(&project_id)
        .bind(user_id)
        .bind(name)
        .bind(kind.as_str())
        .bind(now)
        .bind(now)
        .bind(i64::from(managed))
        .execute(&self.pool)
        .await?;
        self.get_project(user_id, &project_id)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("project {project_id} after insert")))
    }

    async fn rename_project(&self, user_id: &str, project_id: &str, name: &str) -> Result<(), DbError> {
        let result = sqlx::query("UPDATE projects SET name = ?, updated_at = ? WHERE project_id = ? AND user_id = ?")
            .bind(name)
            .bind(now_ms())
            .bind(project_id)
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("project {project_id}")));
        }
        Ok(())
    }

    async fn set_workspace_state(
        &self,
        user_id: &str,
        project_id: &str,
        config_path: Option<&str>,
        default_pe_id: Option<&str>,
        dirty: bool,
    ) -> Result<(), DbError> {
        let result = sqlx::query(
            "UPDATE projects SET workspace_config_path = ?, workspace_default_pe_id = ?, \
             workspace_dirty = ?, updated_at = ? WHERE project_id = ? AND user_id = ?",
        )
        .bind(config_path)
        .bind(default_pe_id)
        .bind(i64::from(dirty))
        .bind(now_ms())
        .bind(project_id)
        .bind(user_id)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("project {project_id}")));
        }
        Ok(())
    }

    async fn replace_roots(
        &self,
        user_id: &str,
        project_id: &str,
        roots: &[RootSpec],
        role: Role,
    ) -> Result<Vec<ProjectExplorerRow>, DbError> {
        let now = now_ms();
        let mut tx = self.pool.begin().await?;
        let existing = sqlx::query_as::<_, ProjectExplorerRow>(&format!(
            "SELECT {ENTRY_COLS} FROM project_explorer WHERE project_id = ? AND owner_user_id = ?"
        ))
        .bind(project_id)
        .bind(user_id)
        .fetch_all(&mut *tx)
        .await?;

        let mut ordered = Vec::with_capacity(roots.len());
        for (position, root) in roots.iter().enumerate() {
            let order_index = position as i64;
            // Reuse the row when the folder survives: its pe_id is the frontend's tab identity.
            if let Some(row) = existing.iter().find(|row| row.folder_id == root.folder_id) {
                sqlx::query(
                    "UPDATE project_explorer SET role = ?, display_name = ?, order_index = ?, updated_at = ? \
                     WHERE pe_id = ? AND owner_user_id = ?",
                )
                .bind(role.as_str())
                .bind(root.display_name.as_deref())
                .bind(order_index)
                .bind(now)
                .bind(&row.pe_id)
                .bind(user_id)
                .execute(&mut *tx)
                .await?;
                let mut updated = row.clone();
                updated.role = role.as_str().to_owned();
                updated.display_name = root.display_name.clone();
                updated.order_index = order_index;
                updated.updated_at = now;
                ordered.push(updated);
                continue;
            }

            let pe_id = generate_prefixed_id("pe");
            sqlx::query(
                "INSERT INTO project_explorer \
                 (pe_id, project_id, owner_user_id, folder_id, role, display_name, order_index, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&pe_id)
            .bind(project_id)
            .bind(user_id)
            .bind(&root.folder_id)
            .bind(role.as_str())
            .bind(root.display_name.as_deref())
            .bind(order_index)
            .bind(now)
            .bind(now)
            .execute(&mut *tx)
            .await?;
            ordered.push(ProjectExplorerRow {
                pe_id,
                project_id: project_id.to_owned(),
                folder_id: root.folder_id.clone(),
                role: role.as_str().to_owned(),
                display_name: root.display_name.clone(),
                order_index,
                created_at: now,
                updated_at: now,
            });
        }

        // Anything the caller dropped from the workspace stops being a root of this project.
        for row in &existing {
            if !roots.iter().any(|root| root.folder_id == row.folder_id) {
                sqlx::query("DELETE FROM project_explorer WHERE pe_id = ? AND owner_user_id = ?")
                    .bind(&row.pe_id)
                    .bind(user_id)
                    .execute(&mut *tx)
                    .await?;
            }
        }

        tx.commit().await?;
        Ok(ordered)
    }
}
