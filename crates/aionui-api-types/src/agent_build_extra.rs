use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::TeamMcpStdioConfig;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionMcpTransport {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: HashMap<String, String>,
    },
    Http {
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
    },
    Sse {
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
    },
    StreamableHttp {
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMcpServer {
    pub id: String,
    pub name: String,
    pub transport: SessionMcpTransport,
}

/// The fork spec a forked conversation carries in `conversations.extra.fork`
/// until its backend session materializes: the parent lineage (UI display) plus
/// the snapshot the first open needs to fork the backend session. Written ONLY
/// by the server-side fork API (`create()` strips a client-supplied `fork` key);
/// once the fork completes and a session id is persisted, this degrades to pure
/// lineage display data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkSpec {
    /// The conversation this one was forked from (lineage display; soft
    /// reference — the parent may be deleted later).
    pub parent_conversation_id: String,
    /// The fork-point message in the PARENT conversation (inclusive).
    pub parent_message_id: String,
    /// The parent's backend session id, snapshotted AT FORK TIME (the parent
    /// may rotate/advance afterwards; the fork must not chase it).
    pub parent_session_id: String,
    /// Backend turn anchor for an at-turn fork (codex `lastTurnId`); `None` =
    /// fork at HEAD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_turn_id: Option<String>,
}

/// The folders a conversation's agent keeps the files it creates in, chosen by
/// the client when the conversation starts inside a user's own workspace and
/// stored in `conversations.extra.session_files`. Other keys the client stores
/// next to these (such as the parent folder) are ignored here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFiles {
    /// Scripts, scratch code, test data and other intermediate files.
    pub work_dir: String,
    /// Finished standalone deliverables the user asked for.
    pub output_dir: String,
}

/// Read `session_files` without ever failing the surrounding build: a value of
/// the wrong shape, or one with an empty path, is treated as absent. A strict
/// field would turn one bad key into "Invalid ACP build options" for ACP and
/// into dropped settings for aionrs, whose parser falls back to defaults.
fn lenient_session_files<'de, D>(deserializer: D) -> Result<Option<SessionFiles>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value
        .and_then(|value| serde_json::from_value::<SessionFiles>(value).ok())
        .filter(|files| !files.work_dir.trim().is_empty() && !files.output_dir.trim().is_empty()))
}

/// ACP-specific fields extracted from `extra` in build task options.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AcpBuildExtra {
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub backend: Option<String>,
    #[serde(default)]
    pub cli_path: Option<String>,
    #[serde(default)]
    pub agent_name: Option<String>,
    #[serde(default)]
    pub custom_agent_id: Option<String>,
    #[serde(default)]
    pub preset_context: Option<String>,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub preset_assistant_id: Option<String>,
    #[serde(default)]
    pub session_mode: Option<String>,
    #[serde(default)]
    pub current_model_id: Option<String>,
    #[serde(default)]
    pub thought_level: Option<String>,
    #[serde(default)]
    pub cron_job_id: Option<String>,
    #[serde(default)]
    pub team_mcp_stdio_config: Option<TeamMcpStdioConfig>,
    #[serde(default)]
    pub mcp_server_ids: Option<Vec<String>>,
    #[serde(default)]
    pub session_mcp_servers: Vec<SessionMcpServer>,
    #[serde(default)]
    pub user_id: Option<String>,
    /// Present only on a forked conversation (see [`ForkSpec`]). Consumed by
    /// the first session open when no backend session id is bound yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork: Option<ForkSpec>,
    /// Where the agent keeps the files it creates (see [`SessionFiles`]).
    #[serde(
        default,
        deserialize_with = "lenient_session_files",
        skip_serializing_if = "Option::is_none"
    )]
    pub session_files: Option<SessionFiles>,
}

/// Aionrs-specific fields extracted from `extra` in build task options.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AionrsBuildExtra {
    #[serde(default)]
    pub system_prompt: Option<String>,
    #[serde(default)]
    pub preset_rules: Option<String>,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub max_turns: Option<usize>,
    #[serde(default)]
    pub max_tool_call_malformed_turns: Option<usize>,
    #[serde(default)]
    pub max_tool_call_failure_turns: Option<usize>,
    #[serde(default)]
    pub session_mode: Option<String>,
    #[serde(default)]
    pub thought_level: Option<String>,
    #[serde(default)]
    pub team_mcp_stdio_config: Option<TeamMcpStdioConfig>,
    #[serde(default)]
    pub mcp_server_ids: Option<Vec<String>>,
    #[serde(default)]
    pub session_mcp_servers: Vec<SessionMcpServer>,
    #[serde(default)]
    pub backend: Option<String>,
    #[serde(default)]
    pub user_id: Option<String>,
    /// Present only on a forked conversation (see [`ForkSpec`]). Consumed by
    /// the aionrs factory on first open when no session exists for this
    /// conversation yet: the parent's session (keyed by
    /// `parent_session_id` = parent conversation id) is copied into this
    /// conversation's session id, cut at `last_turn_id` when the fork was
    /// taken mid-history (`None` = fork at HEAD).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork: Option<ForkSpec>,
    /// Where the agent keeps the files it creates (see [`SessionFiles`]).
    #[serde(
        default,
        deserialize_with = "lenient_session_files",
        skip_serializing_if = "Option::is_none"
    )]
    pub session_files: Option<SessionFiles>,
}

/// ACP model information returned by the ACP backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpModelInfo {
    pub model_id: String,
    pub model_name: Option<String>,
    pub provider: Option<String>,
}

/// Controls what happens when a slash command produces an empty turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlashCommandCompletionBehavior {
    Normal,
    NeutralTipOnEmpty,
}

/// A slash command item available in a conversation session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlashCommandItem {
    pub command: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_behavior: Option<SlashCommandCompletionBehavior>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub empty_turn_tip_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub empty_turn_tip_params: Option<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acp_build_extra_defaults_thought_level_to_none() {
        let parsed: AcpBuildExtra = serde_json::from_str(r#"{"backend":"codex"}"#).unwrap();
        assert!(parsed.thought_level.is_none());
    }

    #[test]
    fn acp_build_extra_parses_thought_level_seed() {
        let parsed: AcpBuildExtra = serde_json::from_str(r#"{"backend":"codex","thought_level":"high"}"#).unwrap();
        assert_eq!(parsed.thought_level.as_deref(), Some("high"));
    }

    #[test]
    fn acp_build_extra_ignores_legacy_guide_config_field() {
        let legacy_key = concat!("guide", "_mcp_config");
        let parsed: AcpBuildExtra = serde_json::from_value(serde_json::json!({
            "backend": "claude",
            legacy_key: {"port": 1234, "token": "legacy", "binary_path": "/bin/aioncore"}
        }))
        .unwrap();

        assert_eq!(parsed.backend.as_deref(), Some("claude"));
        let serialized = serde_json::to_value(&parsed).unwrap();
        assert!(
            serialized.get(legacy_key).is_none(),
            "legacy guide config must be ignored, not re-serialized"
        );
    }

    #[test]
    fn session_files_parse_for_both_agent_families() {
        let extra = serde_json::json!({
            "backend": "claude",
            "session_files": {
                "dir": "C:\\repo\\.woody\\sessions\\2026-11-06_fix",
                "work_dir": "C:\\repo\\.woody\\sessions\\2026-11-06_fix\\work",
                "output_dir": "C:\\repo\\.woody\\sessions\\2026-11-06_fix\\output"
            }
        });
        let expected = SessionFiles {
            work_dir: "C:\\repo\\.woody\\sessions\\2026-11-06_fix\\work".to_owned(),
            output_dir: "C:\\repo\\.woody\\sessions\\2026-11-06_fix\\output".to_owned(),
        };

        let acp: AcpBuildExtra = serde_json::from_value(extra.clone()).unwrap();
        let aionrs: AionrsBuildExtra = serde_json::from_value(extra).unwrap();

        assert_eq!(acp.session_files.as_ref(), Some(&expected));
        assert_eq!(aionrs.session_files.as_ref(), Some(&expected));
    }

    #[test]
    fn malformed_session_files_are_dropped_without_failing_the_build_options() {
        for bad in [
            serde_json::json!("C:\\repo"),
            serde_json::json!({ "work_dir": "C:\\repo\\work" }),
            serde_json::json!({ "work_dir": 7, "output_dir": "C:\\repo\\output" }),
            serde_json::json!({ "work_dir": "  ", "output_dir": "C:\\repo\\output" }),
            serde_json::Value::Null,
        ] {
            let extra = serde_json::json!({ "backend": "claude", "preset_context": "rules", "session_files": bad });

            let acp: AcpBuildExtra = serde_json::from_value(extra.clone()).unwrap();
            let aionrs: AionrsBuildExtra = serde_json::from_value(extra).unwrap();

            assert!(acp.session_files.is_none(), "{bad} must be ignored for ACP");
            assert_eq!(
                acp.preset_context.as_deref(),
                Some("rules"),
                "other fields must survive {bad}"
            );
            assert!(aionrs.session_files.is_none(), "{bad} must be ignored for aionrs");
            assert_eq!(
                aionrs.backend.as_deref(),
                Some("claude"),
                "other fields must survive {bad}"
            );
        }
    }

    #[test]
    fn aionrs_build_extra_ignores_legacy_fields() {
        let legacy_key = concat!("guide", "_mcp_config");
        let parsed: AionrsBuildExtra = serde_json::from_value(serde_json::json!({
            "backend": "aionrs",
            "max_tokens": 8192,
            legacy_key: {"port": 1234, "token": "legacy", "binary_path": "/bin/aioncore"}
        }))
        .unwrap();

        assert_eq!(parsed.backend.as_deref(), Some("aionrs"));
        let serialized = serde_json::to_value(&parsed).unwrap();
        assert!(
            serialized.get(legacy_key).is_none(),
            "legacy guide config must be ignored, not re-serialized"
        );
        assert!(
            serialized.get("max_tokens").is_none(),
            "legacy max_tokens must be ignored, not re-serialized"
        );
    }

    #[test]
    fn aionrs_build_extra_round_trips_thought_level_seed() {
        let parsed: AionrsBuildExtra = serde_json::from_value(serde_json::json!({
            "backend": "aionrs",
            "thought_level": "high"
        }))
        .unwrap();

        let serialized = serde_json::to_value(parsed).unwrap();
        assert_eq!(serialized["thought_level"], "high");
    }
}
