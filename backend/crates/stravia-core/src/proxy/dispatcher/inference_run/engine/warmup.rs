use super::*;

pub(super) async fn complete(
    gw: &Gateway,
    request: &Arc<AiRequest>,
    ctx: &RequestContext,
    phase: &mut PhaseTracker,
    generation: &mut GenerationChainRun,
    ledger: &RunLedger,
) -> Response {
    if ctx.cancellation.is_cancelled() {
        return error_response(499, "request cancelled");
    }
    let Some(mut write) = generation.write.take() else {
        return hook_failure_response("warmup requires a Generation Chain Write");
    };
    let ingress = stravia_runtime_contract::protocol::ids::OPEN_RESPONSES_2026_04_24;
    let mut response = AiResponse::new(write.id(), &request.model);
    write.observe_effective(Arc::clone(request));
    if !write.stage(
        &mut response,
        &crate::generation_chain::GenerationSource::Warmup,
        None,
    ) {
        return hook_failure_response("warmup history could not be staged");
    }
    let mut projection = ClientProjectionSession::new(
        Arc::clone(&gw.history_markers),
        generation.principal.clone(),
        ingress,
    );
    let staged_delivery = match projection.project_staged(&mut response, &[]).await {
        Ok(staged) => staged,
        Err(error) => return hook_failure_response(error),
    };
    for next in [Phase::SemanticComplete, Phase::AwaitingDelivery] {
        if let Err(error) = enter_phase(phase, next) {
            return *error;
        }
    }
    ledger.stage_visible_response(ingress, &response);
    let mut delivery = if request.stream.enabled {
        DeliveryAdapter::buffered_stream(ingress, None)
    } else {
        DeliveryAdapter::non_stream(ingress, None)
    };
    delivery.set_response_profile(request, generation.previous_response_id.as_deref());
    let delivered = delivery.deliver_canonical(&response, StatusCode::OK);
    if delivered.progress != BufferedDeliveryProgress::Prepared {
        return delivered.response;
    }
    let observer = ctx
        .extensions
        .get::<crate::interaction_observation::RunObserver>()
        .expect("admitted warmup observer");
    // 沿用真实交付栅栏；断连或编码失败不能发布可续写的空响应。
    CompletionSettlement::buffered(
        staged_delivery,
        Some(PendingGenerationChainWrite {
            write,
            vendor_publications: std::mem::take(&mut generation.vendor_publications),
        }),
        MarkerExecutions::default(),
    )
    .bind_buffered(
        delivered.response,
        gw.clone(),
        projection,
        ledger.clone(),
        observer,
    )
    .await
}
