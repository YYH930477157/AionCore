//! Managed workspaces are addressed by project identity, never by the last cwd.
use super::ConversationService;
use crate::error::ConversationError;
use aionui_ai_agent::{AgentInstance, types::AionrsWorkspaceRoot};
use aionui_common::AgentType;
use aionui_db::models::ConversationRow;
use aionui_project::{ManagedWorkspaceRuntime, ProjectService, RuntimeStatus};
use std::sync::Arc;
use tokio::sync::OwnedRwLockReadGuard;

impl ConversationService {
    fn workspace_service(&self) -> Option<Arc<ProjectService>> {
        self.project_service.read().ok().and_then(|guard| guard.clone())
    }

    pub(crate) async fn prepare_workspace_binding(
        &self,
        user_id: &str,
        agent_type: AgentType,
        extra: &mut serde_json::Value,
    ) -> Result<Option<(String, OwnedRwLockReadGuard<()>)>, ConversationError> {
        let Some(value) = extra.get("workspace_project_id") else {
            return Ok(None);
        };
        if agent_type != AgentType::Aionrs {
            return Err(ConversationError::bad_request("managed workspaces require aionrs"));
        }
        let project_id = value
            .as_str()
            .filter(|id| !id.trim().is_empty())
            .ok_or_else(|| ConversationError::bad_request("workspace_project_id must be a nonempty string"))?
            .to_owned();
        let service = self
            .workspace_service()
            .ok_or_else(|| ConversationError::bad_request("workspace service unavailable"))?;
        let lease = service
            .acquire_workspace_task(user_id, &project_id)
            .await
            .map_err(|err| ConversationError::bad_request(err.to_string()))?;
        let workspace = service
            .managed_workspace(user_id, &project_id)
            .await
            .map_err(|err| ConversationError::bad_request(err.to_string()))?
            .ok_or_else(|| ConversationError::bad_request("workspace is not managed"))?;
        // The saved workspace is authoritative even when the client has stale paths.
        let default = workspace.roots.iter().find(|root| {
            workspace.default_pe_id.as_deref() == Some(root.pe_id.as_str())
                && root.runtime_status == RuntimeStatus::Available
        });
        extra["workspace"] = serde_json::json!(
            default
                .map(|root| root.path.to_string_lossy().into_owned())
                .unwrap_or_default()
        );
        Ok(Some((project_id, lease)))
    }

    pub(crate) async fn acquire_conversation_workspace(
        &self,
        row: &ConversationRow,
    ) -> Result<Option<OwnedRwLockReadGuard<()>>, ConversationError> {
        if self.conversation_workspace(row).await?.is_none() {
            return Ok(None);
        }
        let (Some(service), Some(project_id)) = (self.workspace_service(), row.project_id.as_deref()) else {
            return Ok(None);
        };
        service
            .acquire_workspace_task(&row.user_id, project_id)
            .await
            .map(Some)
            .map_err(|err| ConversationError::bad_request(err.to_string()))
    }

    pub(crate) async fn conversation_workspace(
        &self,
        row: &ConversationRow,
    ) -> Result<Option<ManagedWorkspaceRuntime>, ConversationError> {
        if row.r#type != AgentType::Aionrs.serde_name() {
            return Ok(None);
        }
        let (Some(service), Some(project_id)) = (self.workspace_service(), row.project_id.as_deref()) else {
            return Ok(None);
        };
        service
            .managed_workspace(&row.user_id, project_id)
            .await
            .map_err(|err| ConversationError::bad_request(err.to_string()))
    }

    pub(crate) async fn managed_workspace_bootstrap(
        &self,
        row: &ConversationRow,
    ) -> Result<Option<String>, ConversationError> {
        let Some(workspace) = self.conversation_workspace(row).await? else {
            return Ok(None);
        };
        if let Some(root) = workspace.roots.iter().find(|root| {
            workspace.default_pe_id.as_deref() == Some(root.pe_id.as_str())
                && root.runtime_status == RuntimeStatus::Available
        }) {
            return Ok(Some(root.path.to_string_lossy().into_owned()));
        }
        // Engine initialization still needs a directory. This private directory is
        // never the command cwd: apply_managed_workspace supplies the explicit
        // missing/absent default before any user turn reaches the engine.
        let private = self.workspace_root.join("managed-runtime").join(&row.id);
        std::fs::create_dir_all(&private)
            .map_err(|err| ConversationError::internal(format!("workspace initialization: {err}")))?;
        Ok(Some(private.to_string_lossy().into_owned()))
    }

    pub(crate) async fn apply_managed_workspace(
        &self,
        user_id: &str,
        conversation_id: &str,
        agent: &AgentInstance,
    ) -> Result<(), ConversationError> {
        let AgentInstance::Aionrs(manager) = agent else {
            return Ok(());
        };
        let row = self
            .conversation_repo
            .get(user_id, conversation_id)
            .await?
            .ok_or_else(|| ConversationError::NotFound {
                id: conversation_id.to_owned(),
            })?;
        let Some(workspace) = self.conversation_workspace(&row).await? else {
            return Ok(());
        };
        let default = workspace
            .roots
            .iter()
            .find(|root| workspace.default_pe_id.as_deref() == Some(root.pe_id.as_str()))
            .map(|root| root.path.clone());
        let roots = workspace
            .roots
            .into_iter()
            .map(|root| AionrsWorkspaceRoot {
                id: root.pe_id,
                name: root.display_name,
                path: root.path,
                available: root.runtime_status == RuntimeStatus::Available,
            })
            .collect();
        manager
            .update_workspace_context(default, roots)
            .await
            .map_err(Into::into)
    }
}
