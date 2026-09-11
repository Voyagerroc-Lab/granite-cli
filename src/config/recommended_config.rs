//! Recommended configuration schema for `granite-cli setup --auto`.
//!
//! A recommended configuration says, per launcher, which capabilities can be
//! auto-enabled and which specific catalog models (at which quantization
//! precisions) are recommended to fill each of that capability's model
//! dependency slots.

use alog::{MessageLevel, alog_channel, use_channel};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use_channel!("RCFG");

// Embedded built-in recommended configurations (from build.rs)
include!(concat!(
    env!("OUT_DIR"),
    "/generated_recommended_configs.rs"
));

/// Built-in recommended configurations parsed from the YAML files embedded
/// by `build.rs`.  Each file becomes one entry; parse errors are logged and
/// the offending file is skipped.
pub static BUILTIN_RECOMMENDED_CONFIGS: std::sync::LazyLock<Vec<RecommendedConfiguration>> =
    std::sync::LazyLock::new(|| {
        RECOMMENDED_CONFIG_SOURCES
            .iter()
            .filter_map(|(name, yaml)| {
                match serde_yaml::from_str::<RecommendedConfiguration>(yaml) {
                    Ok(cfg) => Some(cfg),
                    Err(e) => {
                        alog_channel!(
                            MessageLevel::Warning,
                            "Failed to parse recommended config '{}': {}",
                            name,
                            e
                        );
                        None
                    }
                }
            })
            .collect()
    });

// -- Schema structs -------------------------------------------------------------

/// One launcher's curated auto-setup recommendation. `launcher` is the
/// LAUNCHER_REGISTRY key (e.g. "claude"), or the reserved wildcard "*"
/// applied to any launcher without its own specific entry.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RecommendedConfiguration {
    pub launcher: String,
    pub capabilities: Vec<RecommendedCapability>,
}

/// One capability a launcher can auto-enable, and how to fill each of its
/// model dependency slots.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RecommendedCapability {
    /// CAPABILITY_REGISTRY key (e.g. "agent-model").
    pub capability: String,
    /// Keyed by the capability's own `Dependency::Model.config_key` (e.g.
    /// "model_id") -- a capability may have more than one model dependency
    /// slot, each independently recommended.
    pub models: HashMap<String, RecommendedModelSet>,
}

/// A slot's requirements plus its ordered list of admissible candidates.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RecommendedModelSet {
    /// Minimum useful *achieved* context length for this slot, independent
    /// of which candidate is chosen (e.g. a coding-agent capability
    /// inherently needs more headroom than a lightweight one). Meant to be
    /// checked by callers against the *effective* context after hardware
    /// fit, not a model's native max -- that's the caller's job, not this
    /// module's; this module just carries the number.
    pub min_context_length: Option<u64>,
    /// Ordered by preference; callers should try candidates in order and
    /// take the first that resolves.
    pub models: Vec<RecommendedModel>,
}

/// One candidate model and which of its variants are acceptable.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RecommendedModel {
    pub model: StringMatch,
    /// Per-model precision allow-list (e.g. "Q4_K_M", "IQ4_XS"); empty means
    /// any variant is acceptable.
    pub variant_precisions: Vec<StringMatch>,
}

// -- StringMatch ----------------------------------------------------------------

/// Exact match by default; explicit regex escape hatch so a plain
/// version-like string (e.g. "4.2") is never silently misinterpreted as a
/// pattern. In YAML: a bare string deserializes to `Exact`; a mapping
/// `{regex: "..."}` deserializes to `Regex`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum StringMatch {
    Exact(String),
    Regex { regex: String },
}

impl StringMatch {
    /// Whether `s` matches. An invalid regex pattern never matches (rather
    /// than panicking) -- log a warning via this crate's `alog` logging.
    pub fn matches(&self, s: &str) -> bool {
        match self {
            StringMatch::Exact(pattern) => s == pattern,
            StringMatch::Regex { regex: pattern } => match regex::Regex::new(pattern) {
                Ok(re) => re.is_match(s),
                Err(e) => {
                    alog_channel!(
                        MessageLevel::Warning,
                        "Invalid regex pattern '{}': {}",
                        pattern,
                        e
                    );
                    false
                }
            },
        }
    }
}

// -- Merge/lookup helpers -------------------------------------------------------

/// Look up one launcher key's effective config: a user-authored entry wins
/// outright over the built-in entry for that same key (whole-entry
/// replace, not a field-level merge).
fn resolve_entry<'a>(
    key: &str,
    builtin: &'a [RecommendedConfiguration],
    user: &'a HashMap<String, RecommendedConfiguration>,
) -> Option<&'a RecommendedConfiguration> {
    user.get(key)
        .or_else(|| builtin.iter().find(|c| c.launcher == key))
}

/// The effective capability list for one launcher: the launcher's own
/// entry (if any) unioned with the wildcard ("*") entry (if any), with the
/// launcher's own entry winning per-`capability` key when both define the
/// same capability.
pub fn effective_capabilities(
    launcher_type: &str,
    builtin: &[RecommendedConfiguration],
    user: &HashMap<String, RecommendedConfiguration>,
) -> Vec<RecommendedCapability> {
    let specific = resolve_entry(launcher_type, builtin, user);
    let wildcard = resolve_entry("*", builtin, user);

    let mut by_capability: HashMap<String, RecommendedCapability> = HashMap::new();
    if let Some(w) = wildcard {
        for cap in &w.capabilities {
            by_capability.insert(cap.capability.clone(), cap.clone());
        }
    }
    if let Some(s) = specific {
        for cap in &s.capabilities {
            by_capability.insert(cap.capability.clone(), cap.clone());
        }
    }
    by_capability.into_values().collect()
}

// -- Tests ----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- StringMatch tests --------------------------------------------------

    #[test]
    fn exact_match_matches() {
        let m = StringMatch::Exact("foo".to_string());
        assert!(m.matches("foo"));
    }

    #[test]
    fn exact_non_match_does_not_match() {
        let m = StringMatch::Exact("foo".to_string());
        assert!(!m.matches("bar"));
    }

    #[test]
    fn regex_pattern_matches() {
        let m = StringMatch::Regex {
            regex: "fo.*".to_string(),
        };
        assert!(m.matches("foobar"));
        assert!(m.matches("fo"));
    }

    #[test]
    fn regex_pattern_non_match() {
        let m = StringMatch::Regex {
            regex: "fo.*".to_string(),
        };
        assert!(!m.matches("baz"));
    }

    #[test]
    fn invalid_regex_returns_false_not_panic() {
        let m = StringMatch::Regex {
            regex: "[invalid".to_string(),
        };
        assert!(!m.matches("anything"));
    }

    #[test]
    fn exact_version_string_does_not_treat_dot_as_regex() {
        // The footgun-avoidance case: "4.2" as Exact must match only "4.2"
        // and must NOT match "4x2" (which a regex "." would match).
        let m = StringMatch::Exact("4.2".to_string());
        assert!(m.matches("4.2"));
        assert!(!m.matches("4x2"));
        assert!(!m.matches("42"));
        assert!(!m.matches("4.a2"));
    }

    // -- effective_capabilities tests -----------------------------------------

    fn make_config(launcher: &str, capability: &str, model_val: &str) -> RecommendedConfiguration {
        let mut models = HashMap::new();
        models.insert(
            "model_id".to_string(),
            RecommendedModelSet {
                min_context_length: None,
                models: vec![RecommendedModel {
                    model: StringMatch::Exact(model_val.to_string()),
                    variant_precisions: vec![],
                }],
            },
        );
        RecommendedConfiguration {
            launcher: launcher.to_string(),
            capabilities: vec![RecommendedCapability {
                capability: capability.to_string(),
                models,
            }],
        }
    }

    #[test]
    fn user_entry_fully_replaces_builtin_for_same_launcher() {
        // Builtin has capability X with model "builtin-model".
        // User override for the same launcher has capability Y with model "user-model".
        // Result must be exactly Y, not X+Y.
        let builtin = vec![make_config("claude", "cap-x", "builtin-model")];
        let mut user = HashMap::new();
        user.insert(
            "claude".to_string(),
            make_config("claude", "cap-y", "user-model"),
        );

        let caps = effective_capabilities("claude", &builtin, &user);
        assert_eq!(caps.len(), 1);
        assert_eq!(caps[0].capability, "cap-y");
    }

    #[test]
    fn wildcard_applies_to_launcher_with_no_specific_entry() {
        let builtin = vec![make_config("*", "wildcap", "wildcard-model")];
        let user = HashMap::new();

        let caps = effective_capabilities("bob", &builtin, &user);
        assert_eq!(caps.len(), 1);
        assert_eq!(caps[0].capability, "wildcap");
    }

    #[test]
    fn specific_wins_over_wildcard_for_same_capability_key() {
        // Both wildcard and specific define capability "agent-model".
        // The specific launcher's RecommendedCapability must win.
        let mut specific_models = HashMap::new();
        specific_models.insert(
            "model_id".to_string(),
            RecommendedModelSet {
                min_context_length: None,
                models: vec![RecommendedModel {
                    model: StringMatch::Exact("specific-model".to_string()),
                    variant_precisions: vec![],
                }],
            },
        );

        let mut wildcard_models = HashMap::new();
        wildcard_models.insert(
            "model_id".to_string(),
            RecommendedModelSet {
                min_context_length: None,
                models: vec![RecommendedModel {
                    model: StringMatch::Exact("wildcard-model".to_string()),
                    variant_precisions: vec![],
                }],
            },
        );

        let builtin = vec![
            RecommendedConfiguration {
                launcher: "claude".to_string(),
                capabilities: vec![RecommendedCapability {
                    capability: "agent-model".to_string(),
                    models: specific_models,
                }],
            },
            RecommendedConfiguration {
                launcher: "*".to_string(),
                capabilities: vec![RecommendedCapability {
                    capability: "agent-model".to_string(),
                    models: wildcard_models,
                }],
            },
        ];
        let user = HashMap::new();

        let caps = effective_capabilities("claude", &builtin, &user);
        assert_eq!(caps.len(), 1);
        // Verify the specific launcher's model value wins
        let model_slot = caps[0].models.get("model_id").expect("model_id present");
        assert_eq!(
            model_slot.models[0].model,
            StringMatch::Exact("specific-model".to_string())
        );
    }

    #[test]
    fn wildcard_capabilities_union_with_specific() {
        // Specific launcher defines "agent-model", wildcard defines
        // "sub-agent-cap". Both should appear in the result.
        let builtin = vec![
            make_config("claude", "agent-model", "specific-model"),
            make_config("*", "sub-agent-cap", "wildcard-model"),
        ];
        let user = HashMap::new();

        let caps = effective_capabilities("claude", &builtin, &user);
        assert_eq!(caps.len(), 2);
        let names: Vec<&str> = caps.iter().map(|c| c.capability.as_str()).collect();
        assert!(names.contains(&"agent-model"));
        assert!(names.contains(&"sub-agent-cap"));
    }

    #[test]
    fn no_builtin_no_user_returns_empty() {
        let builtin: Vec<RecommendedConfiguration> = vec![];
        let user = HashMap::new();
        let caps = effective_capabilities("claude", &builtin, &user);
        assert!(caps.is_empty());
    }

    // -- BUILTIN_RECOMMENDED_CONFIGS tests ------------------------------------

    #[test]
    fn builtin_recommended_configs_is_non_empty() {
        assert!(
            !BUILTIN_RECOMMENDED_CONFIGS.is_empty(),
            "BUILTIN_RECOMMENDED_CONFIGS must contain at least one entry"
        );
    }

    #[test]
    fn builtin_contains_claude_agent_model() {
        // The built-in configs must include a claude entry whose capabilities
        // contain an agent-model capability.
        let mut found_claude = false;
        for cfg in &*BUILTIN_RECOMMENDED_CONFIGS {
            if cfg.launcher == "claude" {
                found_claude = true;
                let agent_model = cfg
                    .capabilities
                    .iter()
                    .find(|c| c.capability == "agent-model");
                assert!(
                    agent_model.is_some(),
                    "claude config must define 'agent-model' capability"
                );
                let agent_model = agent_model.unwrap();
                assert!(
                    agent_model.models.contains_key("model_id"),
                    "claude agent-model must have a 'model_id' slot"
                );
            }
        }
        assert!(
            found_claude,
            "BUILTIN_RECOMMENDED_CONFIGS must contain a 'claude' launcher entry"
        );
    }

    #[test]
    fn builtin_contains_wildcard() {
        assert!(
            BUILTIN_RECOMMENDED_CONFIGS
                .iter()
                .any(|c| c.launcher == "*"),
            "must have a wildcard (*) entry"
        );
    }

    #[test]
    fn builtin_contains_opencode() {
        assert!(
            BUILTIN_RECOMMENDED_CONFIGS
                .iter()
                .any(|c| c.launcher == "opencode"),
            "must have an 'opencode' launcher entry"
        );
    }
}
