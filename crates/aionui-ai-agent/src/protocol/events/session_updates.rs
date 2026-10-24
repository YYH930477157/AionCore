use agent_client_protocol::schema::v1::AvailableCommand;
use serde::{Deserialize, Serialize};

/// Data for the `AgentStatus` event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentStatusEventData {
    pub backend: String,
    pub status: String,
    #[serde(default)]
    pub agent_name: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
}

/// Data for the `Thinking` event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThinkingEventData {
    pub content: String,
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub duration: Option<u64>,
    #[serde(default)]
    pub status: Option<String>,
}

/// Data for the `Plan` event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanEventData {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub entries: Vec<serde_json::Value>,
}

/// Data for the `AvailableCommands` event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AvailableCommandsEventData {
    pub commands: Vec<AvailableCommand>,
}

/// Data for the `SkillSuggest` event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillSuggestEventData {
    #[serde(default)]
    pub cron_job_id: Option<String>,
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub skill_content: Option<String>,
}

/// Data for the `CronTrigger` event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronTriggerEventData {
    pub cron_job_id: String,
    pub cron_job_name: String,
    pub triggered_at: i64,
}

/// Data for the `usage` event: one completed LLM call of the in-process aionrs
/// engine, with the context occupancy it leaves behind. It feeds the chat status
/// bar (generation speed, prompt cache hit rate, context ring) and is a side
/// signal, never a message of its own.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageEventData {
    /// Provider input of the call, including cache-read and cache-creation tokens.
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    /// Best-known context occupancy after the call.
    pub context_usage: u64,
    /// Whether `context_usage` is the provider's exact report rather than an estimate.
    pub context_provider_reported: bool,
    /// Configured context window of the model.
    pub context_window: u64,
    /// Wall time from sending the request until the response stream ended.
    pub duration_ms: u64,
}
