use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Normalized model metadata. Missing support is unknown, not unsupported.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelReasoningCapability {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supported: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub efforts: Option<Vec<String>>,
}

pub const MANUAL_REASONING_EFFORTS: &[&str] = &["minimal", "low", "medium", "high", "xhigh"];
pub const WIRE_REASONING_EFFORTS: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh"];

/// Read only recognized effort metadata, leaving malformed metadata unknown.
pub fn reported_model_reasoning(fields: &Map<String, Value>) -> Option<ModelReasoningCapability> {
    if let Some(reasoning) = fields.get("reasoning").and_then(Value::as_object) {
        let mandatory = reasoning.get("mandatory").and_then(Value::as_bool) == Some(true);
        let efforts = match reasoning.get("supported_efforts") {
            None => return Some(unsupported()),
            Some(Value::Null) => WIRE_REASONING_EFFORTS.iter().map(|s| (*s).to_owned()).collect(),
            Some(Value::Array(values)) if values.is_empty() => return Some(unsupported()),
            Some(Value::Array(values)) => {
                let normalized: Vec<String> = WIRE_REASONING_EFFORTS
                    .iter()
                    .filter(|level| values.iter().any(|value| value.as_str() == Some(**level)))
                    .map(|s| (*s).to_owned())
                    .collect();
                if normalized.is_empty() {
                    return None;
                }
                normalized
            }
            _ => return None,
        };
        let efforts: Vec<String> = efforts.into_iter().filter(|s| !mandatory || s != "none").collect();
        return Some(ModelReasoningCapability {
            supported: Some(!efforts.is_empty()),
            efforts: Some(efforts),
        });
    }
    let parameters = fields.get("supported_parameters")?.as_array()?;
    if !parameters.iter().all(Value::is_string) {
        return None;
    }
    Some(ModelReasoningCapability {
        supported: Some(
            parameters
                .iter()
                .any(|value| matches!(value.as_str(), Some("reasoning" | "reasoning_effort"))),
        ),
        efforts: None,
    })
}

/// Provider metadata takes precedence over the conservative built-in catalog.
pub fn resolve_model_reasoning(model: &str, reported: Option<&ModelReasoningCapability>) -> ModelReasoningCapability {
    if let Some(cap) = reported {
        if cap.supported == Some(false) {
            return unsupported();
        }
        if let Some(efforts) = cap.efforts.as_ref() {
            let efforts: Vec<String> = WIRE_REASONING_EFFORTS
                .iter()
                .filter(|level| efforts.iter().any(|value| value == **level))
                .map(|s| (*s).to_owned())
                .collect();
            return ModelReasoningCapability {
                supported: Some(!efforts.is_empty()),
                efforts: Some(efforts),
            };
        }
    }
    let local = local_model_reasoning(model);
    if reported.is_some_and(|cap| cap.supported == Some(true)) && local.supported != Some(true) {
        return ModelReasoningCapability {
            supported: Some(true),
            efforts: None,
        };
    }
    local
}

impl ModelReasoningCapability {
    /// Unknown models expose manual choices without claiming detected support.
    pub fn selectable_efforts(&self) -> Vec<String> {
        if self.supported == Some(false) {
            Vec::new()
        } else {
            self.efforts
                .clone()
                .unwrap_or_else(|| MANUAL_REASONING_EFFORTS.iter().map(|s| (*s).to_owned()).collect())
        }
    }
}

fn unsupported() -> ModelReasoningCapability {
    ModelReasoningCapability {
        supported: Some(false),
        efforts: Some(Vec::new()),
    }
}

fn local_model_reasoning(model: &str) -> ModelReasoningCapability {
    let model = model.strip_prefix("openai/").unwrap_or(model);
    // Official model pages, checked 2026-11-11. Do not guess future/renamed models.
    let rules: &[(&str, &[&str])] = &[
        ("gpt-5", &["minimal", "low", "medium", "high"]),
        ("gpt-5-mini", &["minimal", "low", "medium", "high"]),
        ("gpt-5-nano", &["minimal", "low", "medium", "high"]),
        ("gpt-5.1", &["none", "low", "medium", "high"]),
        ("gpt-5.2", &["none", "low", "medium", "high", "xhigh"]),
        ("o3", &["low", "medium", "high"]),
        ("gpt-4o", &[]),
        ("gpt-4o-mini", &[]),
        ("gpt-4.1", &[]),
        ("gpt-4.1-mini", &[]),
        ("gpt-4.1-nano", &[]),
    ];
    for (id, efforts) in rules {
        if model == *id
            || model
                .strip_prefix(id)
                .and_then(|rest| rest.strip_prefix('-'))
                .is_some_and(is_date_snapshot)
        {
            return ModelReasoningCapability {
                supported: Some(!efforts.is_empty()),
                efforts: Some(efforts.iter().map(|s| (*s).to_owned()).collect()),
            };
        }
    }
    ModelReasoningCapability::default()
}

fn is_date_snapshot(value: &str) -> bool {
    value.len() == 10
        && value.bytes().enumerate().all(|(index, byte)| {
            if index == 4 || index == 7 {
                byte == b'-'
            } else {
                byte.is_ascii_digit()
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn reported(value: Value) -> Option<ModelReasoningCapability> {
        reported_model_reasoning(value.as_object().unwrap())
    }

    #[test]
    fn provider_efforts_are_normalized_and_mandatory_excludes_literal_none() {
        let cap = reported(json!({"reasoning": {
            "mandatory": true, "supported_efforts": ["none", "high", "low", "high", "unknown"]
        }}))
        .unwrap();
        assert_eq!(cap.supported, Some(true));
        assert_eq!(cap.efforts.unwrap(), ["low", "high"]);
    }

    #[test]
    fn null_efforts_accept_all_gateway_values_but_absent_efforts_do_not() {
        assert_eq!(
            reported(json!({"reasoning": {"supported_efforts": null}}))
                .unwrap()
                .efforts
                .unwrap(),
            WIRE_REASONING_EFFORTS
        );
        assert_eq!(
            reported(json!({"reasoning": {"mandatory": true}})).unwrap().supported,
            Some(false)
        );
    }

    #[test]
    fn malformed_metadata_does_not_invent_support() {
        for value in [
            json!({}),
            json!({"supported_parameters": "reasoning"}),
            json!({"reasoning": {"supported_efforts": [7, "bogus"]}}),
            json!({"reasoning": {"supported_efforts": false}}),
        ] {
            assert_eq!(reported(value), None);
        }
    }

    #[test]
    fn parameter_list_distinguishes_explicit_unsupported_from_unknown() {
        assert_eq!(
            reported(json!({"supported_parameters": ["temperature"]}))
                .unwrap()
                .supported,
            Some(false)
        );
        assert_eq!(
            reported(json!({"supported_parameters": ["reasoning"]}))
                .unwrap()
                .supported,
            Some(true)
        );
        assert_eq!(
            reported(json!({"reasoning": {"supported_efforts": []}}))
                .unwrap()
                .supported,
            Some(false)
        );
    }

    #[test]
    fn provider_list_overrides_local_rules_and_explicit_denial_wins() {
        let cap = reported(json!({"reasoning": {"supported_efforts": ["high"]}})).unwrap();
        assert_eq!(resolve_model_reasoning("gpt-5", Some(&cap)), cap);
        let denied = ModelReasoningCapability {
            supported: Some(false),
            efforts: None,
        };
        assert_eq!(resolve_model_reasoning("gpt-5.2", Some(&denied)).supported, Some(false));
    }

    #[test]
    fn local_catalog_matches_only_verified_ids_and_date_snapshots() {
        assert_eq!(
            resolve_model_reasoning("gpt-5", None).efforts.unwrap(),
            ["minimal", "low", "medium", "high"]
        );
        assert_eq!(
            resolve_model_reasoning("openai/gpt-5.1-2025-11-13", None)
                .efforts
                .unwrap(),
            ["none", "low", "medium", "high"]
        );
        assert_eq!(
            resolve_model_reasoning("gpt-5.2", None).efforts.unwrap(),
            ["none", "low", "medium", "high", "xhigh"]
        );
        assert_eq!(resolve_model_reasoning("gpt-4o", None).supported, Some(false));
        for model in ["opaque", "gpt-5-custom", "gpt-5.99", "acme/gpt-5", "gpt-4o-thinking"] {
            assert_eq!(
                resolve_model_reasoning(model, None),
                ModelReasoningCapability::default()
            );
        }
    }
}
