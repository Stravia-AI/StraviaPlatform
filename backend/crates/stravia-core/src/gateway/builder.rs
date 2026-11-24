use super::*;

pub struct GatewayBuilder {
    config: GatewayConfig,
    storage: Option<DynStorage>,
    hooks: Vec<Arc<dyn Hook>>,
    tools: Vec<Arc<dyn PlatformTool>>,
    dynamic_tool_loaders: Vec<Arc<dyn crate::hook::DynamicPlatformToolLoader>>,
    mcp_tools: Vec<Arc<dyn McpTool>>,
    mcp_catalog_provider: Option<Arc<dyn crate::mcp::McpCatalogProvider>>,
    agent_definitions: Vec<stravia_runtime_contract::agent::AgentDefinitionSpec>,
    web_provider_factories: Vec<Arc<dyn stravia_web_access::WebProviderFactory>>,
    policy_resolver: Option<Arc<dyn crate::policy::PolicyResolver>>,
    extension_factory: Option<Arc<dyn GatewayExtensionFactory>>,
}

impl GatewayBuilder {
    pub fn new(config: GatewayConfig) -> Self {
        Self {
            config,
            storage: None,
            hooks: Vec::new(),
            tools: Vec::new(),
            dynamic_tool_loaders: Vec::new(),
            mcp_tools: Vec::new(),
            mcp_catalog_provider: None,
            agent_definitions: Vec::new(),
            web_provider_factories: Vec::new(),
            policy_resolver: None,
            extension_factory: None,
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

    pub fn dynamic_tool_loader(
        mut self,
        loader: Arc<dyn crate::hook::DynamicPlatformToolLoader>,
    ) -> Self {
        self.dynamic_tool_loaders.push(loader);
        self
    }

    pub fn mcp_tool(mut self, tool: Arc<dyn McpTool>) -> Self {
        self.mcp_tools.push(tool);
        self
    }

    pub fn mcp_catalog_provider(mut self, provider: Arc<dyn crate::mcp::McpCatalogProvider>) -> Self {
        self.mcp_catalog_provider = Some(provider);
        self
    }

    pub fn extension_factory(mut self, factory: Arc<dyn GatewayExtensionFactory>) -> Self {
        self.extension_factory = Some(factory);
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

    /// Install an external [`crate::policy::PolicyResolver`] enforced fail-closed
    /// across inference, MCP, and platform capability gates.
    pub fn policy_resolver(mut self, resolver: Arc<dyn crate::policy::PolicyResolver>) -> Self {
        self.policy_resolver = Some(resolver);
        self
    }

    pub async fn build(self) -> anyhow::Result<Gateway> {
        let Self {
            config,
            storage,
            mut hooks,
            mut tools,
            mut dynamic_tool_loaders,
            mut mcp_tools,
            mut mcp_catalog_provider,
            agent_definitions,
            web_provider_factories,
            mut policy_resolver,
            extension_factory,
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
        if let Some(factory) = extension_factory {
            let ext = factory.build_extensions(&gateway.background_clone());
            hooks.extend(ext.hooks);
            tools.extend(ext.platform_tools);
            dynamic_tool_loaders.extend(ext.dynamic_tool_loaders);
            mcp_tools.extend(ext.mcp_tools);
            if mcp_catalog_provider.is_none() {
                mcp_catalog_provider = ext.mcp_catalog_provider;
            }
            if policy_resolver.is_none() {
                policy_resolver = ext.policy_resolver;
            }
        }
        if let Some(resolver) = policy_resolver {
            gateway.policy_resolver = resolver;
        }
        if let Some(provider) = mcp_catalog_provider {
            gateway.mcp_catalog_provider = provider;
        }
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
        configure_gateway_extensions(
            &mut gateway,
            hooks,
            tools,
            dynamic_tool_loaders,
            mcp_tools,
            agent_definitions,
        )
        .await?;
        Ok(gateway)
    }
}
