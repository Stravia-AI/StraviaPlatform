use super::*;

pub struct GatewayBuilder {
    config: GatewayConfig,
    storage: Option<DynStorage>,
    hooks: Vec<Arc<dyn Hook>>,
    tools: Vec<Arc<dyn PlatformTool>>,
    mcp_tools: Vec<Arc<dyn McpTool>>,
    agent_definitions: Vec<stravia_runtime_contract::agent::AgentDefinitionSpec>,
    web_provider_factories: Vec<Arc<dyn stravia_web_access::WebProviderFactory>>,
}

impl GatewayBuilder {
    pub fn new(config: GatewayConfig) -> Self {
        Self {
            config,
            storage: None,
            hooks: Vec::new(),
            tools: Vec::new(),
            mcp_tools: Vec::new(),
            agent_definitions: Vec::new(),
            web_provider_factories: Vec::new(),
        }
    }

    pub fn storage(mut self, storage: DynStorage) -> Self {
        self.storage = Some(storage);
        self
    }

    pub fn hook(mut self, hook: Arc<dyn Hook>) -> Self {
        self.hooks.push(hook);
        self
    }

    pub fn platform_tool(mut self, tool: Arc<dyn PlatformTool>) -> Self {
        self.tools.push(tool);
        self
    }
    pub fn mcp_tool(mut self, tool: Arc<dyn McpTool>) -> Self {
        self.mcp_tools.push(tool);
        self
    }

    pub fn agent_definition(
        mut self,
        definition: stravia_runtime_contract::agent::AgentDefinitionSpec,
    ) -> Self {
        self.agent_definitions.push(definition);
        self
    }

    /// Register an external [`stravia_web_access::WebProviderFactory`] for Search/Fetch providers.
    pub fn web_provider_factory(
        mut self,
        factory: Arc<dyn stravia_web_access::WebProviderFactory>,
    ) -> Self {
        self.web_provider_factories.push(factory);
        self
    }

    pub async fn build(self) -> anyhow::Result<Gateway> {
        let Self {
            config,
            storage,
            hooks,
            tools,
            mcp_tools,
            agent_definitions,
            web_provider_factories,
        } = self;
        let mut registry = stravia_web_access::WebProviderRegistry::new();
        for factory in web_provider_factories {
            registry.register(factory).map_err(anyhow::Error::msg)?;
        }
        let mut gateway = if let Some(storage) = storage {
            Gateway::from_storage(config, storage).await?
        } else {
            Gateway::new(config).await?
        };
        gateway.web_provider_registry = Arc::new(registry);
        gateway.generation_chains = generation_chain::GenerationChain::from_turn_chain(
            Arc::clone(&gateway.turn_chains),
            gateway.artifact_store.clone(),
            gateway.runtime_cache.clone(),
            gateway.retention.clone(),
        )
        .with_history_markers(Arc::clone(&gateway.history_markers))
        .with_redaction_references()
        .with_prompt_admissions(gateway.storage.prompt_admissions())
        .with_atomic_state(gateway.storage.vendor_plugins().clone())
        .with_compaction(gateway.compaction.clone());
        gateway.install_model_turn();
        configure_gateway_extensions(&mut gateway, hooks, tools, mcp_tools, agent_definitions)
            .await?;
        Ok(gateway)
    }
}
