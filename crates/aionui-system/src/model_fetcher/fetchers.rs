use std::collections::HashMap;
use std::time::Duration;

use aionui_api_types::ModelInfo;
use serde::Deserialize;
use serde_json::{Map, Value};
use tracing::warn;

use crate::error::SystemError;

use super::FetchConfig;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Smallest reported context window that is taken at face value.
const MIN_CONTEXT_LIMIT: u64 = 1_024;

/// Models plus the context windows the remote API reported for them.
#[derive(Debug, Default)]
pub(crate) struct FetchedModels {
    pub models: Vec<ModelInfo>,
    pub context_limits: HashMap<String, u64>,
}

impl From<Vec<ModelInfo>> for FetchedModels {
    fn from(models: Vec<ModelInfo>) -> Self {
        Self {
            models,
            context_limits: HashMap::new(),
        }
    }
}

/// Dispatch to the appropriate platform-specific fetcher.
pub(crate) async fn fetch_for_platform(
    client: &reqwest::Client,
    config: &FetchConfig,
) -> Result<FetchedModels, SystemError> {
    match config.platform.as_str() {
        "anthropic" | "claude" => fetch_anthropic(client, &config.base_url, &config.api_key)
            .await
            .map(Into::into),
        "gemini" => fetch_gemini(client, &config.base_url, &config.api_key).await,
        "bedrock" => fetch_bedrock(config).await.map(Into::into),
        "vertex-ai" => Ok(vertex_ai_models().into()),
        "new-api" => fetch_new_api(client, &config.base_url, &config.api_key).await,
        "minimax" => Ok(minimax_models().into()),
        "dashscope-coding" => fetch_dashscope_coding(client, &config.base_url, &config.api_key)
            .await
            .map(Into::into),
        _ => fetch_openai_compatible(client, &config.base_url, &config.api_key).await,
    }
}

// ---------------------------------------------------------------------------
// OpenAI-compatible (default)
// ---------------------------------------------------------------------------

/// Response shape for OpenAI `/models` endpoint.
#[derive(Deserialize)]
struct OpenAiModelsResponse {
    data: Vec<OpenAiModel>,
}

#[derive(Deserialize)]
struct OpenAiModel {
    id: String,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

/// Fields that OpenAI-compatible gateways use for a model's context window:
/// OpenRouter and Together (`context_length`), vLLM and SGLang
/// (`max_model_len`), Groq (`context_window`), Mistral (`max_context_length`).
const OPENAI_CONTEXT_FIELDS: &[&str] = &[
    "context_length",
    "max_model_len",
    "context_window",
    "max_context_length",
];

fn reported_context_limit(fields: &Map<String, Value>) -> Option<u64> {
    OPENAI_CONTEXT_FIELDS
        .iter()
        .find_map(|key| fields.get(*key).and_then(token_count))
}

/// Read a positive whole token count given as a JSON number or numeric string.
fn token_count(value: &Value) -> Option<u64> {
    let count = match value {
        Value::Number(number) => number.as_u64().or_else(|| {
            number
                .as_f64()
                .filter(|f| f.fract() == 0.0 && *f > 0.0)
                .map(|f| f as u64)
        }),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }?;
    (count >= MIN_CONTEXT_LIMIT).then_some(count)
}

/// Fetch models from an OpenAI-compatible `/models` endpoint.
pub(super) async fn fetch_openai_compatible(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
) -> Result<FetchedModels, SystemError> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {api_key}"))
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|e| remote_error(&e))?;

    check_response_status(&resp)?;

    let body: OpenAiModelsResponse = resp
        .json()
        .await
        .map_err(|e| SystemError::BadGateway(format!("Failed to parse models response: {e}")))?;

    let mut fetched = FetchedModels::default();
    for model in body.data {
        if let Some(limit) = reported_context_limit(&model.extra) {
            fetched.context_limits.insert(model.id.clone(), limit);
        }
        fetched.models.push(ModelInfo::Id(model.id));
    }
    Ok(fetched)
}

// ---------------------------------------------------------------------------
// Anthropic
// ---------------------------------------------------------------------------

/// Response shape for Anthropic `/v1/models`.
#[derive(Deserialize)]
struct AnthropicModelsResponse {
    data: Vec<AnthropicModel>,
}

#[derive(Deserialize)]
struct AnthropicModel {
    id: String,
}

const ANTHROPIC_FALLBACK_MODELS: &[&str] = &[
    "claude-sonnet-4-20250514",
    "claude-opus-4-20250514",
    "claude-3-7-sonnet-20250219",
];

async fn fetch_anthropic(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
) -> Result<Vec<ModelInfo>, SystemError> {
    let url = format!("{}/v1/models", base_url.trim_end_matches('/'));
    let result = client
        .get(&url)
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await;

    match result {
        Ok(resp) if resp.status().is_success() => {
            let body: AnthropicModelsResponse = resp
                .json()
                .await
                .map_err(|e| SystemError::BadGateway(format!("Failed to parse Anthropic response: {e}")))?;
            Ok(body.data.into_iter().map(|m| ModelInfo::Id(m.id)).collect())
        }
        Ok(resp) => {
            warn!(
                status = %resp.status(),
                "Anthropic models API failed, using fallback list"
            );
            Ok(fallback_models(ANTHROPIC_FALLBACK_MODELS))
        }
        Err(e) => {
            warn!(error = %e, "Anthropic models API unreachable, using fallback list");
            Ok(fallback_models(ANTHROPIC_FALLBACK_MODELS))
        }
    }
}

// ---------------------------------------------------------------------------
// Gemini
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct GeminiModelsResponse {
    models: Vec<GeminiModel>,
}

#[derive(Deserialize)]
struct GeminiModel {
    name: String,
    #[serde(default, rename = "inputTokenLimit")]
    input_token_limit: Option<Value>,
}

const GEMINI_FALLBACK_MODELS: &[&str] = &["gemini-2.5-pro", "gemini-2.5-flash"];

async fn fetch_gemini(client: &reqwest::Client, base_url: &str, api_key: &str) -> Result<FetchedModels, SystemError> {
    let url = format!("{}/v1beta/models?key={api_key}", base_url.trim_end_matches('/'));
    let result = client.get(&url).timeout(REQUEST_TIMEOUT).send().await;

    match result {
        Ok(resp) if resp.status().is_success() => {
            let body: GeminiModelsResponse = resp
                .json()
                .await
                .map_err(|e| SystemError::BadGateway(format!("Failed to parse Gemini response: {e}")))?;
            let mut fetched = FetchedModels::default();
            for model in body.models {
                // Strip "models/" prefix: "models/gemini-2.5-pro" -> "gemini-2.5-pro"
                let id = model.name.strip_prefix("models/").unwrap_or(&model.name).to_owned();
                if let Some(limit) = model.input_token_limit.as_ref().and_then(token_count) {
                    fetched.context_limits.insert(id.clone(), limit);
                }
                fetched.models.push(ModelInfo::Id(id));
            }
            Ok(fetched)
        }
        Ok(resp) => {
            warn!(
                status = %resp.status(),
                "Gemini models API failed, using fallback list"
            );
            Ok(fallback_models(GEMINI_FALLBACK_MODELS).into())
        }
        Err(e) => {
            warn!(error = %e, "Gemini models API unreachable, using fallback list");
            Ok(fallback_models(GEMINI_FALLBACK_MODELS).into())
        }
    }
}

// ---------------------------------------------------------------------------
// Bedrock (AWS SDK)
// ---------------------------------------------------------------------------

async fn fetch_bedrock(config: &FetchConfig) -> Result<Vec<ModelInfo>, SystemError> {
    let bedrock_cfg = config
        .bedrock_config
        .as_ref()
        .ok_or_else(|| SystemError::BadRequest("Bedrock requires bedrockConfig".into()))?;

    let region = aws_sdk_bedrock::config::Region::new(bedrock_cfg.region.clone());

    let sdk_config = match bedrock_cfg.auth_method {
        aionui_api_types::BedrockAuthMethod::AccessKey => {
            let key_id = bedrock_cfg
                .access_key_id
                .as_deref()
                .ok_or_else(|| SystemError::BadRequest("accessKeyId is required".into()))?;
            let secret = bedrock_cfg
                .secret_access_key
                .as_deref()
                .ok_or_else(|| SystemError::BadRequest("secretAccessKey is required".into()))?;

            let creds = aws_sdk_bedrock::config::Credentials::new(
                key_id, secret, None, // session token
                None, // expiry
                "aionui",
            );
            aws_sdk_bedrock::Config::builder()
                .region(region)
                .credentials_provider(creds)
                .build()
        }
        aionui_api_types::BedrockAuthMethod::Profile => {
            let profile = bedrock_cfg.profile.as_deref().unwrap_or("default");
            let aws_cfg = aws_config::from_env()
                .profile_name(profile)
                .region(aws_config::Region::new(bedrock_cfg.region.clone()))
                .load()
                .await;
            aws_sdk_bedrock::Config::new(&aws_cfg)
        }
    };

    let client = aws_sdk_bedrock::Client::from_conf(sdk_config);
    let resp = client
        .list_inference_profiles()
        .send()
        .await
        .map_err(|e| SystemError::BadGateway(format!("Bedrock API error: {e}")))?;

    let profiles = resp.inference_profile_summaries();
    // Filter to only anthropic.claude models per API Spec
    let models: Vec<ModelInfo> = profiles
        .iter()
        .filter(|p| p.inference_profile_id().starts_with("anthropic.claude"))
        .map(|p| ModelInfo::Id(p.inference_profile_id().to_string()))
        .collect();

    Ok(models)
}

// ---------------------------------------------------------------------------
// Hardcoded platforms
// ---------------------------------------------------------------------------

fn vertex_ai_models() -> Vec<ModelInfo> {
    vec![
        ModelInfo::Id("gemini-2.5-pro".into()),
        ModelInfo::Id("gemini-2.5-flash".into()),
    ]
}

fn minimax_models() -> Vec<ModelInfo> {
    vec![
        ModelInfo::Id("MiniMax-Text-01".into()),
        ModelInfo::Id("abab6.5s-chat".into()),
        ModelInfo::Id("abab6.5-chat".into()),
    ]
}

// ---------------------------------------------------------------------------
// new-api (OpenAI-compatible with /v1 enforcement)
// ---------------------------------------------------------------------------

async fn fetch_new_api(client: &reqwest::Client, base_url: &str, api_key: &str) -> Result<FetchedModels, SystemError> {
    let normalized = ensure_v1_path(base_url);
    fetch_openai_compatible(client, &normalized, api_key).await
}

/// Ensure the URL path ends with `/v1`.
fn ensure_v1_path(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    if trimmed.ends_with("/v1") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/v1")
    }
}

// ---------------------------------------------------------------------------
// dashscope-coding (hardcoded + key validation)
// ---------------------------------------------------------------------------

const DASHSCOPE_MODELS: &[&str] = &["qwen-coder-plus", "qwen-coder-turbo"];

async fn fetch_dashscope_coding(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
) -> Result<Vec<ModelInfo>, SystemError> {
    // Validate key by sending a minimal chat completion request
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let body = serde_json::json!({
        "model": DASHSCOPE_MODELS[0],
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 1
    });

    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {api_key}"))
        .json(&body)
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|e| remote_error(&e))?;

    if resp.status().is_client_error() {
        return Err(SystemError::BadGateway(format!(
            "Dashscope API key validation failed: {}",
            resp.status()
        )));
    }

    Ok(fallback_models(DASHSCOPE_MODELS))
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn fallback_models(ids: &[&str]) -> Vec<ModelInfo> {
    ids.iter().map(|id| ModelInfo::Id((*id).to_string())).collect()
}

fn check_response_status(resp: &reqwest::Response) -> Result<(), SystemError> {
    if resp.status().is_success() {
        return Ok(());
    }
    Err(SystemError::BadGateway(format!(
        "Remote API returned {}",
        resp.status()
    )))
}

fn remote_error(e: &reqwest::Error) -> SystemError {
    if e.is_timeout() {
        SystemError::Timeout("Remote API request timed out".into())
    } else {
        SystemError::BadGateway(format!("Remote API request failed: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_v1_path_already_present() {
        assert_eq!(
            ensure_v1_path("https://api.example.com/v1"),
            "https://api.example.com/v1"
        );
    }

    #[test]
    fn ensure_v1_path_missing() {
        assert_eq!(ensure_v1_path("https://api.example.com"), "https://api.example.com/v1");
    }

    #[test]
    fn ensure_v1_path_trailing_slash() {
        assert_eq!(ensure_v1_path("https://api.example.com/"), "https://api.example.com/v1");
    }

    #[test]
    fn ensure_v1_path_with_v1_and_trailing_slash() {
        assert_eq!(
            ensure_v1_path("https://api.example.com/v1/"),
            "https://api.example.com/v1"
        );
    }

    fn fields(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap()
    }

    #[test]
    fn reported_context_limit_reads_known_gateway_fields() {
        let cases = [
            ("context_length", 131_072),
            ("max_model_len", 32_768),
            ("context_window", 8_192),
            ("max_context_length", 128_000),
        ];
        for (key, limit) in cases {
            let mut map = Map::new();
            map.insert(key.to_owned(), Value::from(limit));
            assert_eq!(reported_context_limit(&map), Some(limit), "{key}");
        }
    }

    #[test]
    fn reported_context_limit_accepts_numeric_strings_and_whole_floats() {
        assert_eq!(
            reported_context_limit(&fields(serde_json::json!({"context_length": "65536"}))),
            Some(65_536)
        );
        assert_eq!(
            reported_context_limit(&fields(serde_json::json!({"max_model_len": 40960.0}))),
            Some(40_960)
        );
    }

    #[test]
    fn reported_context_limit_ignores_missing_and_implausible_values() {
        assert_eq!(
            reported_context_limit(&fields(serde_json::json!({"object": "model"}))),
            None
        );
        assert_eq!(
            reported_context_limit(&fields(serde_json::json!({"context_length": 0}))),
            None
        );
        assert_eq!(
            reported_context_limit(&fields(serde_json::json!({"context_length": -1}))),
            None
        );
        assert_eq!(
            reported_context_limit(&fields(serde_json::json!({"context_length": 512}))),
            None
        );
        assert_eq!(
            reported_context_limit(&fields(serde_json::json!({"context_length": 1.5e3}))),
            Some(1_500)
        );
        assert_eq!(
            reported_context_limit(&fields(serde_json::json!({"context_length": "lots"}))),
            None
        );
        assert_eq!(
            reported_context_limit(&fields(serde_json::json!({"context_length": null}))),
            None
        );
    }

    #[test]
    fn vertex_ai_returns_expected_models() {
        let models = vertex_ai_models();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0], ModelInfo::Id("gemini-2.5-pro".into()));
        assert_eq!(models[1], ModelInfo::Id("gemini-2.5-flash".into()));
    }

    #[test]
    fn minimax_returns_expected_models() {
        let models = minimax_models();
        assert_eq!(models.len(), 3);
        assert_eq!(models[0], ModelInfo::Id("MiniMax-Text-01".into()));
    }

    #[test]
    fn fallback_models_builds_model_info_list() {
        let models = fallback_models(&["a", "b", "c"]);
        assert_eq!(models.len(), 3);
        assert_eq!(models[0], ModelInfo::Id("a".into()));
    }
}
