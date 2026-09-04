use super::*;

pub struct GatewayBuilder {
    config: GatewayConfig,
    storage: Option<DynStorage>,
    hooks: Vec<Arc<dyn Hook>>,
    tools: Vec<Arc<dyn PlatformTool>>,
    mcp_tools: Vec<Arc<dyn McpTool>>,
    agent_definitions: Vec<agent::AgentDefinitionSpec>,
    generation_chain_ttl: Duration,
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
            generation_chain_ttl: Duration::from_secs(7 * 24 * 60 * 60),
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

    pub fn agent_definition(mut self, definition: agent::AgentDefinitionSpec) -> Self {
        self.agent_definitions.push(definition);
        self
    }

    pub fn generation_chain_ttl(mut self, ttl: Duration) -> Self {
        self.generation_chain_ttl = ttl;
        self
    }

    pub async fn build(self) -> anyhow::Result<(Gateway, mpsc::Receiver<LogEntry>)> {
        let Self {
            config,
            storage,
            hooks,
            tools,
            mcp_tools,
            agent_definitions,
            generation_chain_ttl,
        } = self;
        let (mut gateway, log_rx) = if let Some(storage) = storage {
            Gateway::from_storage(config, storage).await?
        } else {
            Gateway::new(config).await?
        };
        gateway.generation_chains = generation_chain::GenerationChain::from_turn_chain(
            Arc::clone(&gateway.turn_chains),
            generation_chain_ttl,
            gateway.artifact_store.clone(),
        )
        .with_history_markers(Arc::clone(&gateway.history_markers));
        gateway.install_model_turn();
        configure_gateway_extensions(&mut gateway, hooks, tools, mcp_tools, agent_definitions)
            .await?;
        Ok((gateway, log_rx))
    }
}
