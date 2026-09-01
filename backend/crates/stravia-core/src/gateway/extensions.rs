use super::*;

pub(super) async fn configure_gateway_extensions(
    gateway: &mut Gateway,
    mut hooks: Vec<Arc<dyn Hook>>,
    mut tools: Vec<Arc<dyn PlatformTool>>,
    mut mcp_tools: Vec<Arc<dyn McpTool>>,
    mut agent_definitions: Vec<agent::AgentDefinitionSpec>,
) -> anyhow::Result<()> {
    agent_definitions.push(web_search::local_search_definition());
    agent_definitions.push(media::media_definition());
    gateway
        .agent_definitions
        .synchronize(agent_definitions)
        .await?;
    let web_platform_tools = web_access::internal_platform_tools(gateway);
    let mut runner_tools: Vec<Arc<dyn agent::AgentTool>> = tools
        .iter()
        .map(|tool| {
            Arc::new(agent::PlatformToolAgentAdapter::new(Arc::clone(tool), 1))
                as Arc<dyn agent::AgentTool>
        })
        .collect();
    for tool in &web_platform_tools {
        runner_tools.push(Arc::new(agent::PlatformToolAgentAdapter::with_id(
            Arc::clone(tool),
            agent::VersionedToolId {
                id: tool.id().as_str().to_owned(),
                version: 1,
            },
        )));
    }
    for tool in &mcp_tools {
        runner_tools.push(Arc::new(agent::McpToolAgentAdapter::new(
            Arc::clone(tool),
            1,
        )));
    }
    let report_validator = Arc::new(web_search::SearchReportValidator);
    let local_search_evidence = Arc::new(web_search::LocalSearchEvidenceStore::default());
    let model = Arc::clone(&gateway.model_turn);
    let mut runner = agent::AgentRunner::new(
        gateway.agent_definitions.clone(),
        model,
        runner_tools,
        Arc::clone(&gateway.turn_chains),
    )?
    .with_hook_runtime(gateway.hook_runtime.clone())
    .with_tool_authorizer(Arc::new(GatewayAgentToolAuthorizer {
        storage: Arc::clone(&gateway.storage),
    }))
    .with_artifact_store(gateway.artifact_store.clone())
    .with_run_lifecycles(vec![Arc::new(WebAccessAgentRunLifecycle {
        service: gateway.web_access(),
    })])
    .with_output_validator(
        agent::AgentDefinitionId::new(web_search::LOCAL_SEARCH_DEFINITION_ID),
        web_search::LOCAL_SEARCH_DEFINITION_REVISION,
        Arc::new(web_search::LocalSearchOutputValidator::new(
            Arc::clone(&report_validator),
            Arc::clone(&local_search_evidence),
        )),
    )
    .with_capability_model_authorization(
        agent::AgentDefinitionId::new(web_search::LOCAL_SEARCH_DEFINITION_ID),
        web_search::LOCAL_SEARCH_DEFINITION_REVISION,
        agent::CapabilityModelAuthorization::WebSearch,
    )
    .with_capability_model_authorization(
        agent::AgentDefinitionId::new(media::MEDIA_DEFINITION_ID),
        media::MEDIA_DEFINITION_REVISION,
        agent::CapabilityModelAuthorization::MediaUnderstanding,
    );
    if let Some(store) = gateway.media_derivatives.as_ref() {
        runner = runner.with_output_validator(
            agent::AgentDefinitionId::new(media::MEDIA_DEFINITION_ID),
            media::MEDIA_DEFINITION_REVISION,
            Arc::new(media::MediaReportValidator::new(Arc::clone(store))),
        );
    }
    if let Some(store) = gateway.media_derivatives.as_ref() {
        *gateway.media_understanding.write().await = Some(media::MediaUnderstandingService::new(
            runner.clone(),
            Arc::clone(store),
        ));
    }
    let definitions = gateway.agent_definitions.list().await;
    for record in &definitions {
        runner.validate_definition_tools(&record.spec)?;
    }
    let public_definitions = gateway.agent_definitions.list_public().await;
    if !public_definitions.is_empty() {
        hooks.push(Arc::new(agent::AgentDefinitionHook::new(
            gateway.agent_definitions.clone(),
        )));
    }
    for record in public_definitions {
        tools.push(Arc::new(agent::AgentCallPlatformTool::new(
            record.spec.id.clone(),
            record.spec.slug.as_str(),
            record.spec.description.clone(),
            runner.clone(),
        )));
        mcp_tools.push(Arc::new(agent::AgentCallMcpTool::new(
            record.spec.id,
            record.spec.slug.as_str(),
            record.spec.description,
            runner.clone(),
            gateway.clone(),
        )));
    }
    let search_runner = web_search::WebSearchRunner::new(
        Arc::new(web_search::SettingsWebSearchConfigStore::new(Arc::clone(
            &gateway.storage,
        ))),
        Arc::clone(&gateway.turn_chains),
        Arc::new(web_search::LocalSearchBackend::new(
            runner,
            local_search_evidence,
        )),
        Arc::new(web_search::CodexAgenticSearchBackend::new(gateway.clone())),
        report_validator,
        Duration::from_secs(7 * 24 * 60 * 60),
        Arc::new(GatewayWebSearchAuthorizer {
            storage: Arc::clone(&gateway.storage),
        }),
    );
    *gateway.web_search_runner_state.write().await = Some(search_runner);
    gateway.hook_runtime = hook_runtime_with_web_search(gateway, hooks, tools)?;
    gateway.mcp_registry = mcp_registry_with_web_search(gateway, mcp_tools)?;
    Ok(())
}
struct GatewayWebSearchAuthorizer {
    storage: storage::DynStorage,
}

fn web_search_authorization_error() -> web_search::WebSearchError {
    web_search::WebSearchError::new("authorization_failed", "Web Search authorization failed")
}

#[async_trait::async_trait]
impl web_search::SearchRunAuthorizer for GatewayWebSearchAuthorizer {
    async fn authorize(
        &self,
        principal: &hook::Principal,
        binding: &web_search::ResolvedWebSearchBackend,
    ) -> Result<(), web_search::WebSearchError> {
        proxy::security::Security::new(self.storage.auth())
            .authorize_principal_web_search(principal)
            .await
            .map_err(|_| web_search_authorization_error())?;
        match binding {
            web_search::ResolvedWebSearchBackend::Local { model_id } => {
                self.storage
                    .routes()
                    .list_active()
                    .await
                    .map_err(|_| web_search_authorization_error())?
                    .into_iter()
                    .find(|route| route.id == *model_id)
                    .ok_or_else(web_search_authorization_error)?;
                proxy::security::Security::new(self.storage.auth())
                    .authorize_principal_capability(principal)
                    .await
                    .map_err(|_| web_search_authorization_error())?;
            }
            web_search::ResolvedWebSearchBackend::Codex {
                provider_id,
                upstream_model,
            } => {
                let provider = self
                    .storage
                    .providers()
                    .get(provider_id)
                    .await
                    .map_err(|_| web_search_authorization_error())?
                    .filter(web_search::codex_provider_contract)
                    .ok_or_else(web_search_authorization_error)?;
                let model_available = self
                    .storage
                    .provider_models()
                    .get(provider_id, upstream_model)
                    .await
                    .map_err(|_| web_search_authorization_error())?
                    .is_some_and(|model| {
                        model.model_id == *upstream_model && model.effective_available()
                    });
                let credential_available = self
                    .storage
                    .oauth_credentials()
                    .get(&provider.id)
                    .await
                    .map_err(|_| web_search_authorization_error())?
                    .is_some();
                if !model_available || !credential_available {
                    return Err(web_search_authorization_error());
                }
            }
        }
        Ok(())
    }
}

struct GatewayAgentToolAuthorizer {
    storage: storage::DynStorage,
}

fn agent_tool_authorization_error() -> agent::AgentRunError {
    agent::AgentRunError::new(
        "tool_authorization_failed",
        "Agent Tool authorization failed",
    )
}

#[async_trait::async_trait]
impl agent::AgentToolAuthorizer for GatewayAgentToolAuthorizer {
    async fn authorize(
        &self,
        principal: &hook::Principal,
        definition_id: &agent::AgentDefinitionId,
        model_id: &str,
    ) -> Result<(), agent::AgentRunError> {
        let model = self
            .storage
            .routes()
            .list_active()
            .await
            .map_err(|_| agent_tool_authorization_error())?
            .into_iter()
            .find(|route| route.id == model_id)
            .ok_or_else(agent_tool_authorization_error)?;
        let security = crate::proxy::security::Security::new(self.storage.auth());
        let capability_owned = definition_id.as_str() == web_search::LOCAL_SEARCH_DEFINITION_ID
            || definition_id.as_str() == media::MEDIA_DEFINITION_ID;
        if capability_owned {
            security.authorize_principal_capability(principal).await
        } else {
            security.authorize_principal_model(principal, &model).await
        }
        .map_err(|_| agent_tool_authorization_error())?;
        Ok(())
    }
}

struct WebAccessAgentRunLifecycle {
    service: web_access::WebAccessService,
}

struct WebAccessAgentRunGuard {
    service: web_access::WebAccessService,
    run_id: String,
}

impl agent::AgentRunGuard for WebAccessAgentRunGuard {}

impl Drop for WebAccessAgentRunGuard {
    fn drop(&mut self) {
        self.service.release_run_snapshot(&self.run_id);
    }
}

#[async_trait::async_trait]
impl agent::AgentRunLifecycle for WebAccessAgentRunLifecycle {
    async fn start(
        &self,
        principal: &hook::Principal,
        run_id: &agent::AgentTurnId,
    ) -> Result<Box<dyn agent::AgentRunGuard>, agent::AgentRunError> {
        self.service
            .capture_run_snapshot(run_id.as_str(), principal.api_key_id())
            .await
            .map_err(|error| {
                agent::AgentRunError::new("web_access_unavailable", error.to_string())
            })?;
        Ok(Box::new(WebAccessAgentRunGuard {
            service: self.service.clone(),
            run_id: run_id.as_str().to_owned(),
        }))
    }
}

fn hook_runtime_with_web_search(
    gateway: &Gateway,
    mut hooks: Vec<Arc<dyn Hook>>,
    mut tools: Vec<Arc<dyn PlatformTool>>,
) -> anyhow::Result<HookRuntime> {
    let (builtin_hooks, builtin_tools) = web_search::builtin_extensions(gateway);
    hooks.extend(builtin_hooks);
    tools.extend(builtin_tools);
    tools.extend(media::platform_tools(gateway));
    hooks.push(media::planning_hook(gateway));

    let mut hook_ids = std::collections::HashSet::new();
    for hook in &hooks {
        let descriptor = hook.descriptor();
        if descriptor.id.as_str().trim().is_empty() {
            anyhow::bail!("hook id cannot be empty");
        }
        if !hook_ids.insert(descriptor.id.as_str().to_string()) {
            anyhow::bail!("duplicate hook id: {}", descriptor.id);
        }
    }
    let tool_registry = PlatformToolRegistry::new(tools).map_err(anyhow::Error::new)?;
    Ok(HookRuntime::with_tools(hooks, tool_registry))
}
fn mcp_registry_with_web_search(
    gateway: &Gateway,
    mut tools: Vec<Arc<dyn McpTool>>,
) -> anyhow::Result<McpToolRegistry> {
    tools.extend(web_search::mcp_tools(gateway));
    tools.extend(media::mcp_tools(gateway));
    McpToolRegistry::new(tools)
}
