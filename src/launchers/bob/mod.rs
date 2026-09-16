// Standard
use std::collections::HashSet;
use std::path::PathBuf;

// Third Party
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

// Local
use crate::capabilities::{
    ApiType, Binding, BindingRequest, BindingType, Capability, McpBinding, SubAgentBinding,
    SubAgentBindingRequest,
};
use crate::launchers::base::HasLauncherMetadata as HasBobLauncherMetadata;
use crate::launchers::base::{EnvBinding, LaunchContext, Launcher, LauncherMetadata, run_command};
use crate::launchers::shared::mcp_cli::{
    mcp_binding_request, register_mcp_server, remove_mcp_server,
};
use crate::registry::ConfigConstructable;
use crate::utils::resolve_shell_command;
use crate::utils::subserver::SubServer;
use crate::utils::ui::Ui;

mod delegate;

/*-- public --*/

#[derive(Debug, Clone, Serialize, Deserialize, Default, schemars::JsonSchema)]
pub struct BobLauncherConfig {
    /// Override path to the `bob` binary for non-PATH installs.
    /// Leave unset to use PATH lookup.
    #[serde(default)]
    pub command_path: Option<String>,

    /// Override path to the `pi` binary used to run any bound sub-agents
    /// (see `bob/delegate.rs`). Leave unset to use PATH lookup, falling back
    /// to a cached/downloaded copy if `pi` isn't found there either. Distinct
    /// from `command_path`, which overrides `bob` itself -- the two binaries
    /// are unrelated.
    #[serde(default)]
    pub pi_command_path: Option<String>,
}

pub struct BobLauncher {
    instance_id: String,
    config: BobLauncherConfig,
    /// `(server_name, binding)` for every MCP-capable capability bound to
    /// this launcher, registered/removed around `run_command` in `launch()`.
    bound_mcp_bindings: Vec<(String, McpBinding)>,
    /// `(tool_name, binding)` for every SubAgent-capable capability bound to
    /// this launcher. Not registered directly -- `launch()` wraps all of
    /// these into a single in-process MCP server (`bob/delegate.rs`) and
    /// registers *that* server instead.
    pending_sub_agents: Vec<(String, SubAgentBinding)>,
}

impl ConfigConstructable for BobLauncher {
    type Config = BobLauncherConfig;

    fn new(
        instance_id: &str,
        cfg: &serde_json::Value,
        _global_config: &crate::config::Config,
    ) -> Self {
        let config: BobLauncherConfig = serde_json::from_value(cfg.clone()).unwrap_or_default();
        Self {
            instance_id: instance_id.to_string(),
            config,
            bound_mcp_bindings: vec![],
            pending_sub_agents: vec![],
        }
    }
}

impl crate::registry::Named for BobLauncher {
    fn instance_id(&self) -> &str {
        &self.instance_id
    }
}

#[async_trait]
impl Launcher for BobLauncher {
    fn name(&self) -> &str {
        "Bob CLI"
    }

    fn command(&self) -> &str {
        self.config.command_path.as_deref().unwrap_or("bob")
    }

    async fn bind_capability(&mut self, capability: &dyn Capability) -> anyhow::Result<()> {
        let supported = Self::metadata().supported_capabilities;
        let capability_types = capability.binding_types();
        if !capability_types.is_subset(&supported) {
            anyhow::bail!(
                "capability supports {:?} which this launcher does not support",
                capability_types.difference(&supported).collect::<Vec<_>>()
            );
        }

        if capability_types.contains(&BindingType::SubAgent) {
            let request = BindingRequest::SubAgent(SubAgentBindingRequest {
                api_type: ApiType::OpenAI,
            });
            let binding = capability.bind(request).await?;
            match binding {
                Binding::SubAgent(binding) => {
                    self.pending_sub_agents
                        .push((capability.instance_id().to_string(), binding));
                }
                other => {
                    anyhow::bail!(
                        "expected a SubAgent binding, got {:?}",
                        other.binding_type()
                    )
                }
            }
            return Ok(());
        }

        let binding = capability.bind(mcp_binding_request()).await?;
        match binding {
            Binding::Mcp(binding) => {
                self.bound_mcp_bindings
                    .push((capability.instance_id().to_string(), binding));
            }
            other => anyhow::bail!("expected an Mcp binding, got {:?}", other.binding_type()),
        }
        Ok(())
    }

    fn validate_command(&self) -> anyhow::Result<PathBuf> {
        resolve_shell_command(&self.config.command_path, "bob")
    }

    async fn env_overlay(&self, _ctx: &LaunchContext) -> anyhow::Result<Vec<EnvBinding>> {
        Ok(vec![])
    }

    /// Registers each bound MCP server with `bob mcp add-json` (scoped to
    /// this workspace) before exec'ing, and best-effort removes them again
    /// afterwards.
    async fn launch(
        &self,
        args: &[String],
        ctx: &LaunchContext,
        ui: &dyn Ui,
    ) -> anyhow::Result<std::process::ExitStatus> {
        let binary = self.validate_command()?;
        let overlay = self.env_overlay(ctx).await?;

        let mut delegate_server: Option<SubServer> = None;
        let mut all_mcp_bindings: Vec<(String, McpBinding)> = self.bound_mcp_bindings.clone();
        if !self.pending_sub_agents.is_empty() {
            let (binding, server) = delegate::start_delegate_mcp_server(
                self.pending_sub_agents.clone(),
                &self.config.pi_command_path,
                ctx,
                ui,
            )
            .await?;
            all_mcp_bindings.push(("bob-sub-agents".to_string(), binding));
            delegate_server = Some(server);
        }

        const SCOPE: &[&str] = &["-s", "workspace"];
        for (name, binding) in &all_mcp_bindings {
            register_mcp_server(&binary, name, binding, SCOPE, ctx, ui)?;
        }

        let result = run_command(binary.clone(), &overlay, args, ctx, ui).await;

        for (name, _) in &all_mcp_bindings {
            remove_mcp_server(&binary, name, SCOPE, ctx, ui);
        }

        if let Some(server) = delegate_server {
            server.shutdown().await;
        }

        result
    }
}

impl HasBobLauncherMetadata for BobLauncher {
    fn metadata() -> LauncherMetadata {
        LauncherMetadata {
            name: "Bob CLI".to_string(),
            description: "IBM Bob AI assistant CLI".to_string(),
            default_command: "bob".to_string(),
            supported_capabilities: HashSet::from([BindingType::Mcp, BindingType::SubAgent]),
            tags: vec!["bob".to_string(), "ibm".to_string()],
        }
    }
}

/*-- tests --*/

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_defaults_to_bob() {
        let l = BobLauncher::new(
            "my-bob",
            &serde_json::json!({}),
            &crate::config::Config::default(),
        );
        assert_eq!(l.command(), "bob");
    }

    #[test]
    fn command_uses_explicit_path_when_set() {
        let l = BobLauncher::new(
            "my-bob",
            &serde_json::json!({
                "command_path": "/opt/bin/bob"
            }),
            &crate::config::Config::default(),
        );
        assert_eq!(l.command(), "/opt/bin/bob");
    }

    #[test]
    fn validate_command_err_for_nonexistent_explicit_path() {
        let l = BobLauncher::new(
            "my-bob",
            &serde_json::json!({
                "command_path": "/no/such/path/bob"
            }),
            &crate::config::Config::default(),
        );
        assert!(l.validate_command().is_err());
    }

    #[test]
    fn validate_command_falls_back_to_path_for_bare_command_name() {
        let l = BobLauncher::new(
            "my-bob",
            &serde_json::json!({
                "command_path": "ls"
            }),
            &crate::config::Config::default(),
        );
        assert!(l.validate_command().is_ok());
    }

    #[test]
    fn metadata_name_is_bob_cli() {
        let meta = BobLauncher::metadata();
        assert_eq!(meta.name, "Bob CLI");
        assert_eq!(meta.default_command, "bob");
    }

    #[test]
    fn config_schema_is_present() {
        use crate::launchers::base::LauncherFactory;
        let mut factory = LauncherFactory::new();
        factory.register::<BobLauncher>("bob");
        let schema = factory.config_schema("bob").unwrap();
        let props = schema.get("properties").and_then(|p| p.as_object());
        assert!(props.is_some());
        let props = props.unwrap();
        assert!(props.contains_key("command_path"));
        assert!(props.contains_key("pi_command_path"));
    }

    #[test]
    fn pi_command_path_is_independent_of_bobs_own_command_path() {
        // Regression guard: overriding bob's own binary must not also be
        // treated as an override for the `pi` binary the sub-agent delegate
        // server resolves -- the two are unrelated commands.
        let l = BobLauncher::new(
            "my-bob",
            &serde_json::json!({ "command_path": "/opt/bin/bob" }),
            &crate::config::Config::default(),
        );
        assert_eq!(l.config.command_path, Some("/opt/bin/bob".to_string()));
        assert_eq!(l.config.pi_command_path, None);
    }

    #[test]
    fn metadata_supported_capabilities_contains_mcp_and_sub_agent() {
        let meta = BobLauncher::metadata();
        assert!(meta.supported_capabilities.contains(&BindingType::Mcp));
        assert!(meta.supported_capabilities.contains(&BindingType::SubAgent));
    }

    // -- bind_capability routing -------------------------------------------

    struct FakeMcpCapability;

    impl crate::registry::Named for FakeMcpCapability {
        fn instance_id(&self) -> &str {
            "fake-mcp"
        }
    }

    #[async_trait]
    impl Capability for FakeMcpCapability {
        fn name(&self) -> &str {
            "Fake Mcp"
        }
        fn description(&self) -> &str {
            "test double"
        }
        fn binding_types(&self) -> HashSet<BindingType> {
            HashSet::from([BindingType::Mcp])
        }
        async fn bind(&self, _request: BindingRequest) -> anyhow::Result<Binding> {
            Ok(Binding::Mcp(McpBinding::Http {
                url: "http://127.0.0.1:1/mcp".to_string(),
                headers: Default::default(),
            }))
        }
    }

    struct FakeSubAgentCapability;

    impl crate::registry::Named for FakeSubAgentCapability {
        fn instance_id(&self) -> &str {
            "fake-sub-agent"
        }
    }

    #[async_trait]
    impl Capability for FakeSubAgentCapability {
        fn name(&self) -> &str {
            "Fake SubAgent"
        }
        fn description(&self) -> &str {
            "test double"
        }
        fn binding_types(&self) -> HashSet<BindingType> {
            HashSet::from([BindingType::SubAgent])
        }
        async fn bind(&self, _request: BindingRequest) -> anyhow::Result<Binding> {
            Ok(Binding::SubAgent(SubAgentBinding {
                description: "explores the repo".to_string(),
                prompt: "You explore things.".to_string(),
                tools: vec![],
                model: crate::capabilities::AgentModelBinding {
                    api_type: ApiType::OpenAI,
                    provider_name: "my-ollama".to_string(),
                    base_url: "http://localhost:11434".to_string(),
                    model_name: "granite4.1:8b".to_string(),
                    endpoint_path: "/v1/chat/completions".to_string(),
                    api_key: None,
                    verify_ssl: true,
                    context_length: Some(131072),
                    custom_headers: None,
                },
                known_type: None,
            }))
        }
    }

    fn bob() -> BobLauncher {
        BobLauncher::new(
            "my-bob",
            &serde_json::json!({}),
            &crate::config::Config::default(),
        )
    }

    #[tokio::test]
    async fn bind_capability_with_sub_agent_only_capability_routes_into_pending_sub_agents() {
        let mut l = bob();
        l.bind_capability(&FakeSubAgentCapability).await.unwrap();
        assert!(l.bound_mcp_bindings.is_empty());
        assert_eq!(l.pending_sub_agents.len(), 1);
        assert_eq!(l.pending_sub_agents[0].0, "fake-sub-agent");
        assert_eq!(l.pending_sub_agents[0].1.description, "explores the repo");
    }

    #[tokio::test]
    async fn bind_capability_with_mcp_only_capability_still_populates_bound_mcp_bindings() {
        let mut l = bob();
        l.bind_capability(&FakeMcpCapability).await.unwrap();
        assert_eq!(l.bound_mcp_bindings.len(), 1);
        assert_eq!(l.bound_mcp_bindings[0].0, "fake-mcp");
        assert!(l.pending_sub_agents.is_empty());
    }
}
