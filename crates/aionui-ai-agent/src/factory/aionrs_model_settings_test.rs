use aion_config::compat::OpenAiApiMode;
use aion_types::message::ImageInputCapability;
use aionui_api_types::ModelReasoningEffort;

use crate::capability::image_input::resolve_image_input_capability;

use super::{
    resolve_aionrs_url_and_compat_with_mode, resolve_context_window, resolve_model_compat_overrides,
    resolve_thought_level,
};

#[test]
fn model_settings_resolve_explicit_vision_and_api_overrides() {
    let overrides = resolve_model_compat_overrides(
        "gpt-5.6-sol",
        r#"{
            "gpt-5.6-sol": {
                "image_input": "supported",
                "openai_api_mode": "chat_completions",
                "context_limit": 131072
            }
        }"#,
    )
    .unwrap();

    assert_eq!(overrides.image_input, Some(ImageInputCapability::Supported));
    assert_eq!(overrides.openai_api_mode, Some(OpenAiApiMode::ChatCompletions));
    assert_eq!(overrides.context_limit, Some(131_072));
}

#[test]
fn missing_model_settings_keep_vision_and_api_automatic() {
    let overrides = resolve_model_compat_overrides("gpt-5.6-sol", r#"{"gpt-4o":{"image_input":"supported"}}"#).unwrap();

    assert_eq!(overrides.image_input, None);
    assert_eq!(overrides.openai_api_mode, None);
    assert_eq!(overrides.context_limit, None);
    assert_eq!(overrides.reasoning_effort, None);
}

#[test]
fn model_reasoning_effort_defaults_resolve_for_each_level() {
    for (level, expected) in [
        ("low", ModelReasoningEffort::Low),
        ("medium", ModelReasoningEffort::Medium),
        ("high", ModelReasoningEffort::High),
    ] {
        let raw = serde_json::json!({"model": {"reasoning_effort": level}}).to_string();
        let settings = resolve_model_compat_overrides("model", &raw).unwrap();
        assert_eq!(settings.reasoning_effort, Some(expected));
        assert_eq!(
            resolve_thought_level(None, settings.reasoning_effort).as_deref(),
            Some(level)
        );
    }
}

#[test]
fn session_reasoning_effort_overrides_the_model_default() {
    assert_eq!(
        resolve_thought_level(Some(" low "), Some(ModelReasoningEffort::High)).as_deref(),
        Some("low")
    );
    assert_eq!(
        resolve_thought_level(Some("auto"), Some(ModelReasoningEffort::High)),
        None
    );
    assert_eq!(
        resolve_thought_level(Some(" auto "), Some(ModelReasoningEffort::High)),
        None
    );
}

#[test]
fn explicit_none_model_default_is_not_dropped_or_replaced_by_high() {
    let settings = resolve_model_compat_overrides("model", r#"{"model":{"reasoning_effort":"none"}}"#).unwrap();
    assert_eq!(
        resolve_thought_level(None, settings.reasoning_effort).as_deref(),
        Some("none")
    );
    assert_eq!(
        resolve_thought_level(Some("none"), Some(ModelReasoningEffort::High)).as_deref(),
        Some("none")
    );
}

#[test]
fn missing_or_blank_session_reasoning_effort_uses_the_model_default() {
    assert_eq!(resolve_thought_level(None, None), None);
    assert_eq!(resolve_thought_level(Some(""), None), None);
    assert_eq!(
        resolve_thought_level(Some("  "), Some(ModelReasoningEffort::Medium)).as_deref(),
        Some("medium")
    );
}

#[test]
fn context_window_prefers_the_model_setting_over_the_provider_limit() {
    assert_eq!(resolve_context_window(Some(32_768), Some(128_000)), Some(32_768));
    assert_eq!(resolve_context_window(None, Some(128_000)), Some(128_000));
    assert_eq!(resolve_context_window(None, None), None);
}

#[test]
fn context_window_treats_non_positive_values_as_unset() {
    assert_eq!(resolve_context_window(Some(0), Some(64_000)), Some(64_000));
    assert_eq!(resolve_context_window(None, Some(0)), None);
    assert_eq!(resolve_context_window(None, Some(-1)), None);
}

#[test]
fn empty_model_settings_preserve_catalog_vision_support() {
    let overrides = resolve_model_compat_overrides("gpt-4o", "{}").unwrap();
    let capability = overrides
        .image_input
        .unwrap_or_else(|| resolve_image_input_capability("openai", Some("https://api.openai.com/v1"), "gpt-4o"));

    assert_eq!(capability, ImageInputCapability::Supported);
}

#[test]
fn unsupported_image_input_explicitly_disables_vision() {
    let overrides = resolve_model_compat_overrides("gpt-4o", r#"{"gpt-4o":{"image_input":"unsupported"}}"#).unwrap();

    assert_eq!(overrides.image_input, Some(ImageInputCapability::Unsupported));
}

#[test]
fn omitted_image_input_in_a_model_entry_keeps_catalog_automatic() {
    let overrides = resolve_model_compat_overrides("gpt-4o", r#"{"gpt-4o":{"openai_api_mode":"responses"}}"#).unwrap();

    assert_eq!(overrides.image_input, None);
    assert_eq!(overrides.openai_api_mode, Some(OpenAiApiMode::Responses));
}

#[test]
fn invalid_model_settings_are_rejected() {
    let result = resolve_model_compat_overrides("gpt-5.6-sol", "not-json");

    assert!(result.is_err());
}

#[test]
fn invalid_model_reasoning_effort_is_rejected() {
    let error = resolve_model_compat_overrides("model", r#"{"model":{"reasoning_effort":"extreme"}}"#)
        .unwrap_err()
        .to_string();

    assert!(error.contains("Invalid model settings config"), "{error}");
    assert!(error.contains("unknown variant `extreme`"), "{error}");
}

#[test]
fn explicit_chat_completions_overrides_gpt_5_6_responses_default() {
    let (base_url, compat) = resolve_aionrs_url_and_compat_with_mode(
        "openai",
        "https://api.openai.com/v1",
        "openai",
        "gpt-5.6-sol",
        false,
        Some(OpenAiApiMode::ChatCompletions),
    );

    assert_eq!(base_url.as_deref(), Some("https://api.openai.com/v1"));
    assert_eq!(compat.api_path.as_deref(), Some("/chat/completions"));
    assert_eq!(compat.openai_api_mode, Some(OpenAiApiMode::ChatCompletions));
}

#[test]
fn explicit_responses_overrides_non_responses_model_default() {
    let (_, compat) = resolve_aionrs_url_and_compat_with_mode(
        "custom",
        "https://proxy.example.com/v1",
        "openai",
        "gpt-4o",
        false,
        Some(OpenAiApiMode::Responses),
    );

    assert_eq!(compat.api_path.as_deref(), Some("/responses"));
    assert_eq!(compat.openai_api_mode, Some(OpenAiApiMode::Responses));
}

#[test]
fn explicit_api_mode_rewrites_known_complete_endpoints_in_both_directions() {
    let (chat_url, chat_compat) = resolve_aionrs_url_and_compat_with_mode(
        "custom",
        "https://proxy.example.com/v1/responses",
        "openai",
        "gpt-5.6-sol",
        true,
        Some(OpenAiApiMode::ChatCompletions),
    );
    let (responses_url, responses_compat) = resolve_aionrs_url_and_compat_with_mode(
        "custom",
        "https://proxy.example.com/v1/chat/completions",
        "openai",
        "gpt-4o",
        true,
        Some(OpenAiApiMode::Responses),
    );

    assert_eq!(
        chat_url.as_deref(),
        Some("https://proxy.example.com/v1/chat/completions")
    );
    assert_eq!(chat_compat.openai_api_mode, Some(OpenAiApiMode::ChatCompletions));
    assert_eq!(responses_url.as_deref(), Some("https://proxy.example.com/v1/responses"));
    assert_eq!(responses_compat.openai_api_mode, Some(OpenAiApiMode::Responses));
}

#[test]
fn explicit_api_mode_keeps_unrecognized_complete_url_but_controls_wire_format() {
    let (base_url, compat) = resolve_aionrs_url_and_compat_with_mode(
        "custom",
        "https://proxy.example.com/generate",
        "openai",
        "gpt-4o",
        true,
        Some(OpenAiApiMode::Responses),
    );

    assert_eq!(base_url.as_deref(), Some("https://proxy.example.com/generate"));
    assert_eq!(compat.api_path.as_deref(), Some(""));
    assert_eq!(compat.openai_api_mode, Some(OpenAiApiMode::Responses));
}
