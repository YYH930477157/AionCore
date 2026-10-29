//! Managed workspace lifecycle. The explorer tables are the durable draft;
//! saved_json is an absolute-path snapshot used only by explicit discard.
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aionui_api_types::{CreateWorkspaceRequest, UpdateWorkspaceRequest, WorkspaceRootRequest, WorkspaceState};
use aionui_common::generate_prefixed_id;
use aionui_db::{ProjectExplorerRow, WorkspaceEntryInput, WorkspaceRow};
use serde::{Deserialize, Serialize};
use tokio::sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};

use super::ProjectService;
use crate::{AttachInput, ProjectDetail, ProjectError, RuntimeStatus, canonical};

#[derive(Debug, Clone)]
pub struct WorkspaceRuntimeRoot {
    pub pe_id: String,
    pub display_name: String,
    pub path: PathBuf,
    pub runtime_status: RuntimeStatus,
}

#[derive(Debug, Clone)]
pub struct ManagedWorkspaceRuntime {
    pub default_pe_id: Option<String>,
    pub roots: Vec<WorkspaceRuntimeRoot>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceFile {
    version: u32,
    project_id: String,
    name: String,
    default_pe_id: Option<String>,
    roots: Vec<WorkspaceFileRoot>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceFileRoot {
    pe_id: String,
    path: String,
    display_name: Option<String>,
}

fn invalid(reason: &str) -> ProjectError {
    ProjectError::InvalidWorkspace { reason: reason.into() }
}

pub(super) fn state_of(row: WorkspaceRow) -> WorkspaceState {
    WorkspaceState {
        managed: true,
        config_path: row.config_path,
        dirty: row.dirty,
        default_pe_id: row.default_pe_id,
    }
}

impl ProjectService {
    fn workspace_lock(&self, project_id: &str) -> Arc<RwLock<()>> {
        self.workspace_locks
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .entry(project_id.into())
            .or_insert_with(|| Arc::new(RwLock::new(())))
            .clone()
    }

    pub(super) fn workspace_write_lock(&self, project_id: &str) -> Result<OwnedRwLockWriteGuard<()>, ProjectError> {
        self.workspace_lock(project_id)
            .try_write_owned()
            .map_err(|_| ProjectError::WorkspaceBusy {
                project_id: project_id.into(),
            })
    }

    /// Keep this guard alive from root resolution through the entire agent task.
    pub async fn acquire_workspace_task(
        &self,
        user_id: &str,
        project_id: &str,
    ) -> Result<OwnedRwLockReadGuard<()>, ProjectError> {
        self.require_project(user_id, project_id).await?;
        let guard = self.workspace_lock(project_id).read_owned().await;
        self.require_project(user_id, project_id).await?;
        Ok(guard)
    }

    async fn require_project(&self, user_id: &str, project_id: &str) -> Result<(), ProjectError> {
        self.store
            .get_project(user_id, project_id)
            .await?
            .ok_or_else(|| ProjectError::ProjectNotFound {
                project_id: project_id.into(),
            })?;
        Ok(())
    }

    pub async fn managed_workspace(
        &self,
        user_id: &str,
        project_id: &str,
    ) -> Result<Option<ManagedWorkspaceRuntime>, ProjectError> {
        let detail = self.get_project(user_id, project_id).await?;
        let Some(state) = detail.workspace else { return Ok(None) };
        let roots = detail
            .explorer
            .entries
            .into_iter()
            .map(|entry| {
                Ok(WorkspaceRuntimeRoot {
                    pe_id: entry.pe_id.clone(),
                    display_name: entry
                        .display_name
                        .or(entry.folder.default_display_name)
                        .unwrap_or(entry.pe_id),
                    path: canonical::uri_to_path(&entry.folder.resource_uri)?,
                    runtime_status: entry.folder.runtime_status,
                })
            })
            .collect::<Result<Vec<_>, ProjectError>>()?;
        Ok(Some(ManagedWorkspaceRuntime {
            default_pe_id: state.default_pe_id,
            roots,
        }))
    }

    pub async fn workspace_state(&self, user_id: &str, project_id: &str) -> Result<WorkspaceState, ProjectError> {
        let detail = self.get_project(user_id, project_id).await?;
        Ok(detail.workspace.unwrap_or(WorkspaceState {
            managed: false,
            config_path: None,
            dirty: false,
            default_pe_id: (!detail.explorer.workspace_pe_id.is_empty()).then_some(detail.explorer.workspace_pe_id),
        }))
    }

    pub async fn create_workspace(
        &self,
        user_id: &str,
        request: CreateWorkspaceRequest,
    ) -> Result<ProjectDetail, ProjectError> {
        let project_id = generate_prefixed_id("project");
        let mut roots = Vec::new();
        let default_pe_id = if let Some(uri) = request.uri {
            let pe_id = generate_prefixed_id("pe");
            roots.push(WorkspaceRootRequest {
                pe_id: Some(pe_id.clone()),
                uri,
                display_name: None,
            });
            Some(pe_id)
        } else {
            None
        };
        let draft = UpdateWorkspaceRequest {
            name: request.name,
            roots,
            default_pe_id,
        };
        let initial = UpdateWorkspaceRequest {
            name: draft.name.clone(),
            roots: Vec::new(),
            default_pe_id: None,
        };
        let row = WorkspaceRow {
            project_id: project_id.clone(),
            config_path: None,
            dirty: true,
            default_pe_id: draft.default_pe_id.clone(),
            saved_json: Some(serde_json::to_string(&initial).map_err(|_| invalid("cannot serialize initial draft"))?),
        };
        self.persist_workspace(user_id, row, draft).await?;
        tracing::info!(%project_id, "workspace draft created");
        self.get_project(user_id, &project_id).await
    }

    pub async fn update_workspace(
        &self,
        user_id: &str,
        project_id: &str,
        draft: UpdateWorkspaceRequest,
    ) -> Result<ProjectDetail, ProjectError> {
        self.require_project(user_id, project_id).await?;
        let _guard = self.workspace_write_lock(project_id)?;
        // Supplied IDs must already belong to this project. New roots omit IDs.
        let existing = self.store.list_entries(user_id, project_id).await?;
        for root in &draft.roots {
            if let Some(id) = &root.pe_id
                && !existing.iter().any(|(entry, _)| &entry.pe_id == id)
            {
                return Err(invalid("unknown root identity"));
            }
        }
        self.replace_workspace_draft(user_id, project_id, draft).await?;
        self.get_project(user_id, project_id).await
    }

    pub(super) async fn workspace_draft(
        &self,
        user_id: &str,
        project_id: &str,
    ) -> Result<UpdateWorkspaceRequest, ProjectError> {
        let detail = self.get_project(user_id, project_id).await?;
        let default_pe_id = detail.workspace.map(|s| s.default_pe_id).unwrap_or_else(|| {
            (!detail.explorer.workspace_pe_id.is_empty()).then_some(detail.explorer.workspace_pe_id)
        });
        Ok(UpdateWorkspaceRequest {
            name: detail.name,
            default_pe_id,
            roots: detail
                .explorer
                .entries
                .into_iter()
                .map(|entry| WorkspaceRootRequest {
                    pe_id: Some(entry.pe_id),
                    uri: entry.folder.resource_uri,
                    display_name: entry.display_name,
                })
                .collect(),
        })
    }

    pub(super) async fn replace_workspace_draft(
        &self,
        user_id: &str,
        project_id: &str,
        draft: UpdateWorkspaceRequest,
    ) -> Result<(), ProjectError> {
        let mut row = match self.store.get_workspace(user_id, project_id).await? {
            Some(row) => row,
            None => WorkspaceRow {
                project_id: project_id.into(),
                config_path: None,
                dirty: true,
                default_pe_id: None,
                saved_json: Some(
                    serde_json::to_string(&self.workspace_draft(user_id, project_id).await?)
                        .map_err(|_| invalid("cannot serialize original project"))?,
                ),
            },
        };
        row.dirty = true;
        row.default_pe_id = draft.default_pe_id.clone();
        self.persist_workspace(user_id, row, draft).await
    }

    pub(super) async fn attach_managed_folder(
        &self,
        user_id: &str,
        input: AttachInput,
    ) -> Result<ProjectExplorerRow, ProjectError> {
        let mut draft = self.workspace_draft(user_id, &input.project_id).await?;
        let pe_id = generate_prefixed_id("pe");
        draft.roots.push(WorkspaceRootRequest {
            pe_id: Some(pe_id.clone()),
            uri: input.uri,
            display_name: input.display_name,
        });
        self.replace_workspace_draft(user_id, &input.project_id, draft).await?;
        self.store
            .get_entry(user_id, &pe_id)
            .await?
            .ok_or_else(|| invalid("attached root missing"))
    }

    async fn persist_workspace(
        &self,
        user_id: &str,
        row: WorkspaceRow,
        mut draft: UpdateWorkspaceRequest,
    ) -> Result<(), ProjectError> {
        if draft.name.trim().is_empty() || draft.name.len() > 256 || draft.roots.len() > 128 {
            return Err(invalid("name is required (maximum 256 bytes); maximum 128 roots"));
        }
        let mut ids = HashSet::new();
        let mut paths: Vec<PathBuf> = Vec::new();
        let mut prepared = Vec::new();
        for root in &mut draft.roots {
            let pe_id = root.pe_id.get_or_insert_with(|| generate_prefixed_id("pe")).clone();
            if pe_id.is_empty() || !ids.insert(pe_id.clone()) {
                return Err(invalid("duplicate or empty root identity"));
            }
            let canonical = canonical::canonicalize(&root.uri)?;
            let path = actual_path(&canonical::uri_to_path(&root.uri)?)?;
            for previous in &paths {
                if path == *previous {
                    return Err(ProjectError::ProjectExplorerDuplicate {
                        project_id: row.project_id.clone(),
                        folder_id: String::new(),
                    });
                }
                if path.starts_with(previous) || previous.starts_with(&path) {
                    return Err(ProjectError::ProjectExplorerOverlap {
                        project_id: row.project_id.clone(),
                    });
                }
            }
            if std::fs::metadata(&path).is_ok_and(|m| !m.is_dir()) {
                return Err(ProjectError::FolderNotDirectory {
                    path: path.to_string_lossy().into_owned(),
                });
            }
            paths.push(path);
            prepared.push((pe_id, canonical, root.uri.clone(), root.display_name.clone()));
        }
        if draft.default_pe_id.as_ref().is_some_and(|id| !ids.contains(id)) {
            return Err(invalid("default root must be a workspace member"));
        }
        let mut entries = Vec::new();
        for (pe_id, canonical, uri, display_name) in prepared {
            let folder = self.store.upsert_folder(canonical.as_str(), &uri).await?;
            entries.push(WorkspaceEntryInput {
                pe_id,
                folder_id: folder.folder_id,
                display_name,
            });
        }
        let invalidated = self
            .store
            .list_entries(user_id, &row.project_id)
            .await?
            .into_iter()
            .filter(|(old, _)| {
                !entries
                    .iter()
                    .any(|new| new.pe_id == old.pe_id && new.folder_id == old.folder_id)
            })
            .map(|(entry, _)| entry.pe_id)
            .collect();
        self.store
            .put_workspace(user_id, &draft.name, &row, &entries)
            .await
            .map_err(|error| match error {
                aionui_db::DbError::NotFound(_) => ProjectError::ProjectNotFound {
                    project_id: row.project_id.clone(),
                },
                aionui_db::DbError::Conflict(_) => invalid("workspace identity belongs to a different project"),
                error => error.into(),
            })?;
        self.invalidate_fs_roots(invalidated);
        self.notify_roots_changed(&row.project_id, user_id);
        Ok(())
    }

    pub async fn save_workspace(
        &self,
        user_id: &str,
        project_id: &str,
        path: Option<String>,
    ) -> Result<WorkspaceState, ProjectError> {
        self.require_project(user_id, project_id).await?;
        // Saving only snapshots existing membership; it can coexist with a task.
        // The read lease still excludes root updates for the entire file/DB save.
        let _guard = self.acquire_workspace_task(user_id, project_id).await?;
        let draft = self.workspace_draft(user_id, project_id).await?;
        let current = self.store.get_workspace(user_id, project_id).await?;
        let path = path
            .or_else(|| current.and_then(|r| r.config_path))
            .ok_or_else(|| invalid("a path is required for the first save"))?;
        let path = absolute_config_path(&path)?;
        let parent = path.parent().ok_or_else(|| invalid("config path has no parent"))?;
        let roots = draft
            .roots
            .iter()
            .map(|root| {
                let root_path = absolute_config_path(&canonical::uri_to_path(&root.uri)?.to_string_lossy())?;
                let root_identity =
                    canonical::fs_path(&canonical::canonicalize(&canonical::to_file_uri(&root_path)?)?)?;
                let parent_identity = canonical::fs_path(&canonical::canonicalize(&canonical::to_file_uri(parent)?)?)?;
                let portable: PathBuf = if root_identity.starts_with(&parent_identity) {
                    root_path.components().skip(parent.components().count()).collect()
                } else {
                    root_path
                };
                let path = portable.to_string_lossy().replace('\\', "/");
                Ok(WorkspaceFileRoot {
                    pe_id: root.pe_id.clone().unwrap(),
                    path: if path.is_empty() { ".".into() } else { path },
                    display_name: root.display_name.clone(),
                })
            })
            .collect::<Result<Vec<_>, ProjectError>>()?;
        let file = WorkspaceFile {
            version: 1,
            project_id: project_id.into(),
            name: draft.name.clone(),
            default_pe_id: draft.default_pe_id.clone(),
            roots,
        };
        let bytes = serde_json::to_vec_pretty(&file).map_err(|_| invalid("cannot serialize workspace"))?;
        let temp = parent.join(format!(".{}.tmp", generate_prefixed_id("workspace")));
        let write = (|| -> std::io::Result<()> {
            use std::io::Write;
            let mut out = std::fs::OpenOptions::new().write(true).create_new(true).open(&temp)?;
            out.write_all(&bytes)?;
            out.sync_all()?;
            std::fs::rename(&temp, &path)
        })();
        if write.is_err() {
            let _ = std::fs::remove_file(temp);
            return Err(ProjectError::WorkspaceIo);
        }
        let row = WorkspaceRow {
            project_id: project_id.into(),
            config_path: Some(path.to_string_lossy().into_owned()),
            dirty: false,
            default_pe_id: draft.default_pe_id.clone(),
            saved_json: Some(serde_json::to_string(&draft).map_err(|_| invalid("cannot serialize draft"))?),
        };
        self.persist_workspace(user_id, row.clone(), draft).await?;
        tracing::info!(%project_id, "workspace saved");
        Ok(state_of(row))
    }

    pub async fn discard_workspace(&self, user_id: &str, project_id: &str) -> Result<ProjectDetail, ProjectError> {
        self.require_project(user_id, project_id).await?;
        let _guard = self.workspace_write_lock(project_id)?;
        let mut row = self
            .store
            .get_workspace(user_id, project_id)
            .await?
            .ok_or_else(|| invalid("not a managed workspace"))?;
        let draft: UpdateWorkspaceRequest = match &row.saved_json {
            Some(json) => serde_json::from_str(json).map_err(|_| invalid("saved snapshot is invalid"))?,
            None => UpdateWorkspaceRequest {
                name: self.workspace_draft(user_id, project_id).await?.name,
                roots: Vec::new(),
                default_pe_id: None,
            },
        };
        row.dirty = false;
        row.default_pe_id = draft.default_pe_id.clone();
        self.persist_workspace(user_id, row, draft).await?;
        self.get_project(user_id, project_id).await
    }

    pub async fn open_workspace(&self, user_id: &str, path: String) -> Result<ProjectDetail, ProjectError> {
        let path = absolute_config_path(&path)?;
        let metadata = std::fs::metadata(&path).map_err(|_| ProjectError::WorkspaceIo)?;
        if metadata.len() > 1_048_576 {
            return Err(invalid("workspace config exceeds 1 MiB"));
        }
        let bytes = std::fs::read(&path).map_err(|_| ProjectError::WorkspaceIo)?;
        let file: WorkspaceFile =
            serde_json::from_slice(&bytes).map_err(|_| invalid("invalid workspace configuration"))?;
        if file.version != 1 || file.project_id.is_empty() {
            return Err(invalid("unsupported version or empty project identity"));
        }
        let _guard = self.workspace_write_lock(&file.project_id)?;
        let current = self.store.get_workspace(user_id, &file.project_id).await?;
        if current.is_none() && self.store.get_project(user_id, &file.project_id).await?.is_some() {
            return Err(invalid("configuration identity belongs to an existing legacy project"));
        }
        if current.is_some_and(|r| r.dirty) {
            // Opening a file never silently overwrites an existing unsaved draft.
            return self.get_project(user_id, &file.project_id).await;
        }
        let parent = path.parent().ok_or_else(|| invalid("config path has no parent"))?;
        let roots = file
            .roots
            .into_iter()
            .map(|root| {
                let root_path = PathBuf::from(root.path);
                let root_path = if root_path.is_absolute() {
                    root_path
                } else {
                    parent.join(root_path)
                };
                Ok(WorkspaceRootRequest {
                    pe_id: Some(root.pe_id),
                    uri: canonical::to_file_uri(&root_path)?,
                    display_name: root.display_name,
                })
            })
            .collect::<Result<Vec<_>, ProjectError>>()?;
        let draft = UpdateWorkspaceRequest {
            name: file.name,
            roots,
            default_pe_id: file.default_pe_id,
        };
        let row = WorkspaceRow {
            project_id: file.project_id.clone(),
            config_path: Some(path.to_string_lossy().into_owned()),
            dirty: false,
            default_pe_id: draft.default_pe_id.clone(),
            saved_json: Some(serde_json::to_string(&draft).map_err(|_| invalid("cannot serialize draft"))?),
        };
        self.persist_workspace(user_id, row, draft).await?;
        self.get_project(user_id, &file.project_id).await
    }
}

fn absolute_config_path(value: &str) -> Result<PathBuf, ProjectError> {
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err(invalid("configuration path must be absolute"));
    }
    // Keep display casing: the canonical identity intentionally folds casing on
    // Windows, which would break relative-path serialization against real URIs.
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    Ok(normalized)
}

/// Resolve existing ancestors too: missing children below a junction still
/// compare against that junction's target and cannot bypass overlap checks.
fn actual_path(path: &Path) -> Result<PathBuf, ProjectError> {
    let mut prefix = path.to_path_buf();
    let mut suffix = Vec::new();
    let actual = loop {
        match std::fs::canonicalize(&prefix) {
            Ok(path) => break path,
            Err(_) => {
                let Some(leaf) = prefix.file_name() else { break prefix };
                suffix.push(leaf.to_os_string());
                if !prefix.pop() {
                    break prefix;
                }
            }
        }
    };
    let mut actual = actual;
    for part in suffix.into_iter().rev() {
        actual.push(part);
    }
    canonical::fs_path(&canonical::canonicalize(&canonical::to_file_uri(&actual)?)?)
}
