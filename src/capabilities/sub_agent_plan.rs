//! `PlanSubAgentCapability`: defines a named planning sub-agent with a static
//! prompt and fixed tool allow-list (FileRead, Search, FileSearch, Shell,
//! WebFetch, WebSearch -- everything read-only, no FileWrite/FileEdit), and a
//! `Model`/`Provider` of its own. Mirrors `ExploreSubAgentCapability`.

use serde::{Deserialize, Serialize};
use serde_valid::Validate;

use crate::capabilities::base::KnownSubAgent;
use crate::capabilities::base::ToolName;
use crate::declare_sub_agent_basic;

const PLAN_DESCRIPTION: &str = "Explore the codebase and design detailed implementation plans. Use when the user needs a structured plan with steps, file references, and architectural decisions before implementation.";
const PLAN_PROMPT: &str = "You are a software architect and planning specialist. Your role is to explore the codebase and design implementation plans.

=== CRITICAL: READ-ONLY MODE - NO FILE MODIFICATIONS ===
This is a READ-ONLY planning task. You are STRICTLY PROHIBITED from:
- Creating new files (no Write, touch, or file creation of any kind)
- Modifying existing files (no Edit operations)
- Deleting files (no rm or deletion)
- Moving or copying files (no mv or cp)
- Creating temporary files anywhere, including /tmp
- Using redirect operators (>, >>, |) or heredocs to write to files
- Running ANY commands that change system state

Your role is EXCLUSIVELY to explore the codebase and design implementation plans. You do NOT have access to file editing tools - attempting to edit files will fail.

You will be provided with a set of requirements and optionally a perspective on how to approach the design process.

## Your Process

1. **Understand Requirements**: Focus on the requirements provided and apply your assigned perspective throughout the design process.

2. **Explore Thoroughly**:
   - Read any files provided to you in the initial prompt
   - Find existing patterns and conventions using `find`, `grep`, file search / glob, and search
   - Understand the current architecture
   - Identify similar features as reference
   - Trace through relevant code paths
   - Use the shell tool ONLY for read-only operations (`ls, git status, git log, git diff, find, grep, cat, head, tail, git status, git log, git diff`
   - NEVER use the shell tool for: `mkdir, touch, rm, cp, mv, git add, git commit, npm install, pip install, git add, git commit, npm install, pip install`, or any file creation/modification

3. **Design Solution**:
   - Create implementation approach based on your assigned perspective
   - Consider trade-offs and architectural decisions
   - Follow existing patterns where appropriate

4. **Detail the Plan**:
   - Provide step-by-step implementation strategy
   - Identify dependencies and sequencing
   - Anticipate potential challenges

## Required Output

End your response with:

### Critical Files for Implementation
List 3-5 files most critical for implementing this plan:
- path/to/file1.ts
- path/to/file2.ts
- path/to/file3.ts

REMEMBER: You can ONLY explore and plan. You CANNOT and MUST NOT write, edit, or modify any files. You do NOT have access to file editing tools.";

declare_sub_agent_basic!(
    PlanSubAgentCapability
    PlanSubAgentCapabilityConfig
    "Plan Sub-Agent";
    "Defines a named planning sub-agent (static prompt, fixed read-only tools, and model) that a launched coding agent can delegate implementation-plan design to.";
    ["agent", "plan"]
    PLAN_DESCRIPTION.to_string();
    PLAN_PROMPT.to_string();
    vec![
        ToolName::FileRead,
        ToolName::Search,
        ToolName::FileSearch,
        ToolName::Shell,
        ToolName::WebFetch,
        ToolName::WebSearch,
    ];
    Some(KnownSubAgent::Plan)
);

/*-- tests -------------------------------------------------------------------*/

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::base::{
        Binding, BindingRequest, BindingType, Capability, Dependency, HasCapabilityMetadata,
        SubAgentBindingRequest,
    };
    use crate::config::{Config, ModelConfig};
    use crate::models::{Model, ModelFunction};
    use crate::providers::{
        ApiEndpoint, ApiType, HealthStatus, ModelFormat, Provider, ProviderError,
    };
    use crate::registry::ConfigConstructable;
    use crate::registry::Secret;
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::collections::HashSet;
    use std::sync::Arc;

    #[derive(Clone, Default)]
    struct FakeProvider {
        instance_id: String,
        base_url: String,
        api_key: Option<Secret>,
        verify_ssl: bool,
        api_types: Vec<ApiType>,
        endpoints: HashMap<ModelFunction, Vec<ApiEndpoint>>,
        alias: Option<String>,
    }

    impl ConfigConstructable for FakeProvider {
        type Config = crate::registry::NoConfig;
        fn new(_: &str, _: &serde_json::Value, _: &crate::config::Config) -> Self {
            unimplemented!("not used in tests")
        }
    }

    impl crate::registry::Named for FakeProvider {
        fn instance_id(&self) -> &str {
            &self.instance_id
        }
    }

    #[async_trait]
    impl Provider for FakeProvider {
        fn name(&self) -> &str {
            "Fake Provider"
        }
        fn function_endpoints(&self) -> HashMap<ModelFunction, Vec<ApiEndpoint>> {
            self.endpoints.clone()
        }
        fn supported_api_types(&self) -> Vec<ApiType> {
            self.api_types.clone()
        }
        fn base_url(&self) -> &str {
            &self.base_url
        }
        fn api_key(&self) -> Option<&Secret> {
            self.api_key.as_ref()
        }
        fn verify_ssl(&self) -> bool {
            self.verify_ssl
        }
        fn custom_headers(&self) -> Option<std::collections::HashMap<String, Secret>> {
            None
        }
        fn supported_formats(&self) -> Vec<ModelFormat> {
            vec![]
        }
        fn model_alias(
            &self,
            _model_id: String,
            _variant: Option<&crate::models::ModelVariant>,
        ) -> Option<String> {
            self.alias.clone()
        }
        async fn health_check(&self) -> Result<HealthStatus, ProviderError> {
            unimplemented!("not used in tests")
        }
    }

    fn ok_provider() -> FakeProvider {
        let mut endpoints = HashMap::new();
        endpoints.insert(
            ModelFunction::Chat,
            vec![ApiEndpoint::OpenAIChat, ApiEndpoint::AnthropicMessages],
        );
        FakeProvider {
            instance_id: "my-ollama".to_string(),
            base_url: "http://localhost:11434".to_string(),
            api_key: None,
            verify_ssl: true,
            api_types: vec![ApiType::OpenAI, ApiType::Anthropic],
            endpoints,
            alias: None,
        }
    }

    struct TestModel {
        supported_functions: Vec<ModelFunction>,
        provider: FakeProvider,
    }

    impl ConfigConstructable for TestModel {
        type Config = crate::registry::NoConfig;
        fn new(_: &str, _: &serde_json::Value, _: &crate::config::Config) -> Self {
            unimplemented!("not used in tests")
        }
    }

    impl crate::registry::Named for TestModel {
        fn instance_id(&self) -> &str {
            "granite-3.1-8b-instruct"
        }
    }

    impl Model for TestModel {
        fn family(&self) -> &str {
            "Test"
        }
        fn version(&self) -> &str {
            "1.0"
        }
        fn size(&self) -> u64 {
            1
        }
        fn context_length(&self) -> u64 {
            4096
        }
        fn model_type(&self) -> &crate::models::ModelType {
            &crate::models::ModelType::Text
        }
        fn huggingface_repo(&self) -> &str {
            "test/test"
        }
        fn native_dtype(&self) -> &str {
            "bfloat16"
        }
        fn architecture(&self) -> &crate::models::ModelArchitecture {
            unimplemented!("not used in tests")
        }
        fn variants(&self) -> &[crate::models::ModelVariant] {
            &[]
        }
        fn description(&self) -> Option<&str> {
            None
        }
        fn tags(&self) -> &[String] {
            &[]
        }
        fn supported_functions(&self) -> &[ModelFunction] {
            &self.supported_functions
        }
        fn provider(&self) -> anyhow::Result<Box<dyn Provider>> {
            Ok(Box::new(self.provider.clone()))
        }
    }

    fn plan_capability_with_test_model(
        functions: Vec<ModelFunction>,
        provider: FakeProvider,
    ) -> PlanSubAgentCapability {
        let mut config = Config::default();
        config.models.insert(
            "granite-3.1-8b-instruct".to_string(),
            ModelConfig {
                model_id: "granite-3.1-8b-instruct".to_string(),
                model_type: "granite-3.1-8b-instruct".to_string(),
                config: serde_json::json!({}),
                provider_id: "ollama".to_string(),
                variant: None,
            },
        );
        let cap = PlanSubAgentCapability::new(
            "planner",
            &serde_json::json!({
                "model_id": "granite-3.1-8b-instruct",
            }),
            &config,
        );
        PlanSubAgentCapability {
            instance_id: cap.instance_id,
            config: cap.config,
            configured_model: crate::models::ConfiguredModel::for_test(
                Arc::new(TestModel {
                    supported_functions: functions,
                    provider,
                }),
                None,
            ),
            description: cap.description,
            prompt: cap.prompt,
            tools: cap.tools,
        }
    }

    fn request(api_type: ApiType) -> BindingRequest {
        BindingRequest::SubAgent(SubAgentBindingRequest { api_type })
    }

    #[tokio::test]
    async fn bind_succeeds_and_carries_description_prompt_and_tools() {
        let mut config = Config::default();
        config.models.insert(
            "granite-3.1-8b-instruct".to_string(),
            ModelConfig {
                model_id: "granite-3.1-8b-instruct".to_string(),
                model_type: "granite-3.1-8b-instruct".to_string(),
                config: serde_json::json!({}),
                provider_id: "ollama".to_string(),
                variant: None,
            },
        );
        let cap = PlanSubAgentCapability::new(
            "planner",
            &serde_json::json!({
                "model_id": "granite-3.1-8b-instruct",
            }),
            &config,
        );
        let cap = PlanSubAgentCapability {
            instance_id: cap.instance_id,
            config: cap.config,
            configured_model: crate::models::ConfiguredModel::for_test(
                Arc::new(TestModel {
                    supported_functions: vec![ModelFunction::Chat],
                    provider: ok_provider(),
                }),
                None,
            ),
            description: cap.description,
            prompt: cap.prompt,
            tools: cap.tools,
        };

        let binding = cap.bind(request(ApiType::Anthropic)).await.unwrap();
        let Binding::SubAgent(binding) = binding else {
            panic!("expected SubAgent binding")
        };
        assert_eq!(binding.description, PLAN_DESCRIPTION);
        assert_eq!(binding.prompt, PLAN_PROMPT.to_string());
        assert_eq!(
            binding.tools,
            vec![
                ToolName::FileRead,
                ToolName::Search,
                ToolName::FileSearch,
                ToolName::Shell,
                ToolName::WebFetch,
                ToolName::WebSearch,
            ]
        );
        assert_eq!(binding.model.base_url, "http://localhost:11434");
        assert_eq!(binding.model.model_name, "granite-3.1-8b-instruct");
        assert_eq!(binding.model.api_type, ApiType::Anthropic);
    }

    #[test]
    fn binding_types_reports_sub_agent() {
        let cap = plan_capability_with_test_model(vec![ModelFunction::Chat], ok_provider());
        assert_eq!(cap.binding_types(), HashSet::from([BindingType::SubAgent]));
    }

    #[test]
    fn metadata_declares_a_required_model_dependency() {
        let deps = PlanSubAgentCapability::metadata().dependencies;
        assert_eq!(deps.len(), 1);
        assert!(deps.iter().any(|d| matches!(
            d,
            Dependency::Model { config_key, required: true, .. } if config_key == "model_id"
        )));
    }

    #[test]
    fn metadata_reports_supported_binding_types_and_wildcard_dependency() {
        let meta = PlanSubAgentCapability::metadata();
        assert_eq!(
            meta.supported_binding_types,
            HashSet::from([BindingType::SubAgent])
        );
        assert!(meta.dependencies.iter().any(|d| matches!(
            d,
            Dependency::Model {
                resolved_id: None,
                ..
            }
        )));
    }
}
