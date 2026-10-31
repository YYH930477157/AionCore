//! Managed multi-root workspaces.
//!
//! A managed workspace is a project whose roots are an ordered, user-edited list with a mutable
//! default root, saved to a `.woody-workspace` file and reopened from it. It differs from the
//! legacy single-root project in two ways that shape this module:
//!
//! * the roots are mutable as a set (add / remove / reorder / rename), so they are applied with
//!   [`IProjectStore::replace_roots`] rather than one attach at a time, and
//! * the file, not the row, is the durable form: `dirty` means "the rows here differ from that
//!   file", and `discard` re-reads it.
//!
//! The file stores roots by `file://` URI, never by `pe_id`: `pe_id`s are per-database, so a
//! workspace file has to survive being opened by another install.

use std::path::{Path, PathBuf};

use aionui_db::{ProjectKind, Role, RootSpec};
use serde::{Deserialize, Serialize};

use crate::canonical;
use crate::service::ProjectService;
use crate::types::{ProjectDetail, ProjectError, WorkspaceState};

/// Version this build writes. Unknown versions are refused rather than guessed at, because a
/// future file may express roots keyed differently (globbed, remote) than URI-per-root.
const WORKSPACE_FILE_VERSION: u32 = 1;

/// Configuration file extension the desktop's file picker filters on.
pub const WORKSPACE_FILE_EXTENSION: &str = "woody-workspace";

/// On-disk workspace configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceFile {
    pub version: u32,
    pub name: String,
    #[serde(default)]
    pub roots: Vec<WorkspaceFileRoot>,
    /// Index into `roots` of the root new conversations start in. An index (not a `pe_id`) so
    /// the file stays install-independent; out-of-range falls back to the first root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_root: Option<usize>,
}

/// One root in a [`WorkspaceFile`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceFileRoot {
    pub uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

/// One root of an update request: an existing root (by `pe_id`) or a new folder (by `uri`).
#[derive(Debug, Clone)]
pub struct WorkspaceRootInput {
    pub pe_id: Option<String>,
    pub uri: String,
    pub display_name: Option<String>,
}

/// Whole-workspace update. Roots are applied as an ordered set.
#[derive(Debug, Clone)]
pub struct WorkspaceUpdateInput {
    pub name: String,
    pub roots: Vec<WorkspaceRootInput>,
    pub default_pe_id: Option<String>,
}

impl ProjectService {
    /// `POST /api/projects/workspaces` — start a managed workspace, optionally with one root.
    pub async fn create_managed(
        &self,
        user_id: &str,
        name: Option<String>,
        uri: Option<String>,
    ) -> Result<ProjectDetail, ProjectError> {
        let folder = match uri {
            Some(uri) => {
                let canonical = canonical::canonicalize(&uri)?;
                self.ensure_folder_accessible(&canonical)?;
                Some(self.project_store().upsert_folder(canonical.as_str(), &uri).await?)
            }
            None => None,
        };

        let name = name
            .map(|name| name.trim().to_owned())
            .filter(|name| !name.is_empty())
            .or_else(|| folder.as_ref().and_then(default_name_of))
            .unwrap_or_else(|| "Workspace".to_owned());

        let project = self
            .project_store()
            .create_project(user_id, &name, ProjectKind::Standard, true)
            .await?;

        if let Some(folder) = folder {
            let roots = [RootSpec {
                folder_id: folder.folder_id,
                display_name: None,
            }];
            let entries = self
                .project_store()
                .replace_roots(user_id, &project.project_id, &roots, Role::Attached)
                .await?;
            if let Some(first) = entries.first() {
                self.project_store()
                    .set_workspace_state(user_id, &project.project_id, None, Some(&first.pe_id), false)
                    .await?;
            }
            self.notify_scm_roots_changed(&project.project_id, user_id);
        }

        self.get_project(user_id, &project.project_id).await
    }

    /// `POST /api/projects/workspaces/open` — open a `.woody-workspace` file as a new project.
    pub async fn open_managed(&self, user_id: &str, path: &Path) -> Result<ProjectDetail, ProjectError> {
        let file = read_workspace_file(path)?;
        let project = self
            .project_store()
            .create_project(user_id, &workspace_name(&file), ProjectKind::Standard, true)
            .await?;
        self.apply_workspace_file(user_id, &project.project_id, &file, Some(path), false)
            .await?;
        self.notify_scm_roots_changed(&project.project_id, user_id);
        self.get_project(user_id, &project.project_id).await
    }

    /// `GET /api/projects/{id}/workspace` — the managed-workspace state of a project.
    pub async fn workspace_state(&self, user_id: &str, project_id: &str) -> Result<WorkspaceState, ProjectError> {
        let project = self
            .project_store()
            .get_project(user_id, project_id)
            .await?
            .ok_or_else(|| ProjectError::ProjectNotFound {
                project_id: project_id.to_owned(),
            })?;
        Ok(WorkspaceState {
            managed: project.workspace_managed != 0,
            config_path: project.workspace_config_path,
            dirty: project.workspace_dirty != 0,
            default_pe_id: project.workspace_default_pe_id,
        })
    }

    /// `PUT /api/projects/{id}/workspace` — apply an edited root set, marking it unsaved.
    pub async fn update_managed(
        &self,
        user_id: &str,
        project_id: &str,
        update: WorkspaceUpdateInput,
    ) -> Result<ProjectDetail, ProjectError> {
        let project = self.managed_project(user_id, project_id).await?;
        let existing = self.project_store().list_entries(user_id, project_id).await?;
        let roots = self.resolve_update_roots(project_id, &update.roots, &existing).await?;

        let name = update.name.trim();
        if !name.is_empty() && name != project.name {
            self.project_store().rename_project(user_id, project_id, name).await?;
        }

        let entries = self
            .project_store()
            .replace_roots(user_id, project_id, &roots, Role::Attached)
            .await?;
        let default_pe_id = self.resolve_default_root(update.default_pe_id.as_deref(), &entries)?;
        // Only a workspace that already has a file can be "dirty"; until save picks a path the
        // rows are simply the current state.
        let dirty = project.workspace_config_path.is_some();
        self.project_store()
            .set_workspace_state(
                user_id,
                project_id,
                project.workspace_config_path.as_deref(),
                default_pe_id.as_deref(),
                dirty,
            )
            .await?;
        self.notify_scm_roots_changed(project_id, user_id);
        self.get_project(user_id, project_id).await
    }

    /// `POST /api/projects/{id}/workspace/save` — write the configuration file and clear `dirty`.
    pub async fn save_managed(
        &self,
        user_id: &str,
        project_id: &str,
        path: Option<&Path>,
    ) -> Result<WorkspaceState, ProjectError> {
        let project = self.managed_project(user_id, project_id).await?;
        let entries = self.project_store().list_entries(user_id, project_id).await?;

        let target = match path {
            Some(path) => path.to_path_buf(),
            None => project
                .workspace_config_path
                .as_deref()
                .map(PathBuf::from)
                .ok_or(ProjectError::WorkspacePathRequired)?,
        };

        let default_root = project
            .workspace_default_pe_id
            .as_deref()
            .and_then(|pe_id| entries.iter().position(|(entry, _)| entry.pe_id == pe_id));
        let file = WorkspaceFile {
            version: WORKSPACE_FILE_VERSION,
            name: project.name.clone(),
            roots: entries
                .iter()
                .map(|(entry, folder)| WorkspaceFileRoot {
                    uri: folder.resource_uri.clone(),
                    display_name: entry.display_name.clone(),
                })
                .collect(),
            default_root,
        };
        write_workspace_file(&target, &file)?;

        self.project_store()
            .set_workspace_state(
                user_id,
                project_id,
                Some(&target.to_string_lossy()),
                project.workspace_default_pe_id.as_deref(),
                false,
            )
            .await?;
        Ok(WorkspaceState {
            managed: true,
            config_path: Some(target.to_string_lossy().into_owned()),
            dirty: false,
            default_pe_id: project.workspace_default_pe_id,
        })
    }

    /// `POST /api/projects/{id}/workspace/discard` — drop unsaved edits by re-reading the file.
    ///
    /// A workspace that was never saved has nothing to go back to, so discard only clears the
    /// flag there.
    pub async fn discard_managed(&self, user_id: &str, project_id: &str) -> Result<ProjectDetail, ProjectError> {
        let project = self.managed_project(user_id, project_id).await?;
        if let Some(path) = project.workspace_config_path.as_deref() {
            let file = read_workspace_file(Path::new(path))?;
            self.apply_workspace_file(user_id, project_id, &file, Some(Path::new(path)), false)
                .await?;
            self.notify_scm_roots_changed(project_id, user_id);
        } else {
            self.project_store()
                .set_workspace_state(
                    user_id,
                    project_id,
                    None,
                    project.workspace_default_pe_id.as_deref(),
                    false,
                )
                .await?;
        }
        self.get_project(user_id, project_id).await
    }

    /// Fetch a project and refuse the operation when it is not a managed workspace.
    async fn managed_project(&self, user_id: &str, project_id: &str) -> Result<aionui_db::ProjectRow, ProjectError> {
        let project = self
            .project_store()
            .get_project(user_id, project_id)
            .await?
            .ok_or_else(|| ProjectError::ProjectNotFound {
                project_id: project_id.to_owned(),
            })?;
        if project.workspace_managed == 0 {
            return Err(ProjectError::WorkspaceNotManaged {
                project_id: project_id.to_owned(),
            });
        }
        Ok(project)
    }

    /// Resolve update roots to folders: known roots keep their folder, new ones are registered.
    async fn resolve_update_roots(
        &self,
        project_id: &str,
        roots: &[WorkspaceRootInput],
        existing: &[(aionui_db::ProjectExplorerRow, aionui_db::FolderRow)],
    ) -> Result<Vec<RootSpec>, ProjectError> {
        let mut resolved: Vec<RootSpec> = Vec::with_capacity(roots.len());
        for root in roots {
            let folder_id = match root.pe_id.as_deref() {
                Some(pe_id) => existing
                    .iter()
                    .find(|(entry, _)| entry.pe_id == pe_id)
                    .map(|(_, folder)| folder.folder_id.clone())
                    .ok_or_else(|| ProjectError::ProjectExplorerNotFound {
                        pe_id: pe_id.to_owned(),
                    })?,
                None => {
                    let canonical = canonical::canonicalize(&root.uri)?;
                    self.ensure_folder_accessible(&canonical)?;
                    self.project_store()
                        .upsert_folder(canonical.as_str(), &root.uri)
                        .await?
                        .folder_id
                }
            };
            if resolved.iter().any(|spec| spec.folder_id == folder_id) {
                return Err(ProjectError::ProjectExplorerDuplicate {
                    project_id: project_id.to_owned(),
                    folder_id,
                });
            }
            resolved.push(RootSpec {
                folder_id,
                display_name: root.display_name.clone(),
            });
        }
        self.reject_overlapping_roots(project_id, &resolved).await?;
        Ok(resolved)
    }

    /// Reject a root that contains another one: two nested roots would double-list the same files
    /// and break the single-owner rule the containment checks rely on.
    async fn reject_overlapping_roots(&self, project_id: &str, roots: &[RootSpec]) -> Result<(), ProjectError> {
        let mut paths: Vec<std::path::PathBuf> = Vec::with_capacity(roots.len());
        for root in roots {
            let folder = self.project_store().get_folder(&root.folder_id).await?.ok_or_else(|| {
                ProjectError::FolderNotFound {
                    path: root.folder_id.clone(),
                }
            })?;
            let canonical = canonical::canonicalize(&folder.resource_canonical)?;
            paths.push(canonical::fs_path(&canonical)?);
        }
        for (index, path) in paths.iter().enumerate() {
            for (other_index, other) in paths.iter().enumerate() {
                if index != other_index && other.starts_with(path) {
                    return Err(ProjectError::ProjectExplorerOverlap {
                        project_id: project_id.to_owned(),
                    });
                }
            }
        }
        Ok(())
    }

    /// Map a requested default root onto the row that actually exists now.
    fn resolve_default_root(
        &self,
        requested: Option<&str>,
        entries: &[aionui_db::ProjectExplorerRow],
    ) -> Result<Option<String>, ProjectError> {
        match requested {
            Some(pe_id) => {
                if entries.iter().any(|entry| entry.pe_id == pe_id) {
                    Ok(Some(pe_id.to_owned()))
                } else {
                    Err(ProjectError::WorkspaceDefaultRootInvalid {
                        pe_id: pe_id.to_owned(),
                    })
                }
            }
            None => Ok(entries.first().map(|entry| entry.pe_id.clone())),
        }
    }

    /// Replace the project's roots and binding with the contents of a parsed workspace file.
    ///
    /// Shared by `open` and `discard`: both mean "this file is the truth".
    async fn apply_workspace_file(
        &self,
        user_id: &str,
        project_id: &str,
        file: &WorkspaceFile,
        config_path: Option<&Path>,
        dirty: bool,
    ) -> Result<(), ProjectError> {
        // The file is the truth, name included: discard restores it and open is a no-op here.
        self.project_store()
            .rename_project(user_id, project_id, &workspace_name(file))
            .await?;

        let mut roots: Vec<RootSpec> = Vec::with_capacity(file.roots.len());
        for root in &file.roots {
            let canonical = canonical::canonicalize(&root.uri)?;
            let folder = self
                .project_store()
                .upsert_folder(canonical.as_str(), &root.uri)
                .await?;
            if roots.iter().any(|spec| spec.folder_id == folder.folder_id) {
                // A file may legitimately repeat a root after a bad merge; keep the first.
                continue;
            }
            roots.push(RootSpec {
                folder_id: folder.folder_id,
                display_name: root.display_name.clone(),
            });
        }

        let entries = self
            .project_store()
            .replace_roots(user_id, project_id, &roots, Role::Attached)
            .await?;
        let default_pe_id = file
            .default_root
            .and_then(|index| entries.get(index))
            .or_else(|| entries.first())
            .map(|entry| entry.pe_id.clone());
        self.project_store()
            .set_workspace_state(
                user_id,
                project_id,
                config_path.map(|path| path.to_string_lossy().into_owned()).as_deref(),
                default_pe_id.as_deref(),
                dirty,
            )
            .await?;
        Ok(())
    }
}

/// Project name from a workspace file, falling back to a stable placeholder.
fn workspace_name(file: &WorkspaceFile) -> String {
    let name = file.name.trim();
    if name.is_empty() {
        "Workspace".to_owned()
    } else {
        name.to_owned()
    }
}

/// Default project name for a workspace created from one folder.
fn default_name_of(folder: &aionui_db::FolderRow) -> Option<String> {
    canonical::canonicalize(&folder.resource_canonical)
        .ok()
        .map(|canonical| canonical::basename(&canonical))
        .filter(|name| !name.trim().is_empty())
}

/// Read and validate a `.woody-workspace` file.
fn read_workspace_file(path: &Path) -> Result<WorkspaceFile, ProjectError> {
    let text = std::fs::read_to_string(path).map_err(|_| ProjectError::WorkspaceFileUnreadable {
        path: path.to_string_lossy().into_owned(),
    })?;
    let file: WorkspaceFile = serde_json::from_str(&text).map_err(|error| ProjectError::WorkspaceFileInvalid {
        path: path.to_string_lossy().into_owned(),
        reason: error.to_string(),
    })?;
    if file.version != WORKSPACE_FILE_VERSION {
        return Err(ProjectError::WorkspaceFileInvalid {
            path: path.to_string_lossy().into_owned(),
            reason: format!(
                "unsupported version {} (expected {WORKSPACE_FILE_VERSION})",
                file.version
            ),
        });
    }
    Ok(file)
}

/// Write the configuration through a sibling temp file, so a crash cannot leave a half file that
/// `discard` would then treat as truth.
fn write_workspace_file(path: &Path, file: &WorkspaceFile) -> Result<(), ProjectError> {
    let body = serde_json::to_vec_pretty(file).map_err(|error| ProjectError::WorkspaceFileInvalid {
        path: path.to_string_lossy().into_owned(),
        reason: error.to_string(),
    })?;
    let temp = path.with_extension("tmp");
    std::fs::write(&temp, body).map_err(|_| ProjectError::WorkspaceFileUnreadable {
        path: temp.to_string_lossy().into_owned(),
    })?;
    std::fs::rename(&temp, path).map_err(|_| ProjectError::WorkspaceFileUnreadable {
        path: path.to_string_lossy().into_owned(),
    })
}

#[cfg(test)]
#[path = "workspace_test.rs"]
mod workspace_test;
