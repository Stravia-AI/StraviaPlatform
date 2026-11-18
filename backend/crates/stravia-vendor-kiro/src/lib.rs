mod allowance;
mod descriptor;
mod discovery;
mod request;
mod stream;
mod transport;
mod messages {
    include!(concat!(env!("OUT_DIR"), "/messages.rs"));
}

use serde_json::Value;
use stravia_protocol_codec::accumulator::StreamResponseAccumulator;
use stravia_vendor_common::common::plugin_error;
use stravia_vendor_sdk::{
    AiRequest, AiStreamDelta, ConnectionCheckResponse, ConnectionCheckVerification, ErrorKind,
    GuestHost, Operation, OperationInput, OperationOutput, PluginError, ProviderSnapshot,
    VendorDescriptor, VendorGuest,
};
use uuid::Uuid;

const VENDOR_ID: &str = "kiro";
const CHANNEL_ID: &str = "default";
const PROTOCOL: &str = "kiro-generate-assistant-response";
const DEFAULT_BASE_URL: &str = "https://runtime.us-east-1.kiro.dev";

pub struct Kiro;

impl VendorGuest for Kiro {
    fn descriptor() -> VendorDescriptor {
        descriptor::descriptor()
    }

    fn check_connection(
        host: &GuestHost,
        channel: &str,
        provider: ProviderSnapshot,
    ) -> Result<ConnectionCheckResponse, PluginError> {
        check_channel(channel, &provider)?;
        // 只读管理面第一页，不生成内容，不把固定管理面响应误报为推理端点已验证。
        discovery::page(host, &provider, None)?;
        Ok(ConnectionCheckResponse {
            verified: ConnectionCheckVerification::Credentials,
        })
    }

    fn select_protocol(
        operation: Operation,
        channel: &str,
        provider: &ProviderSnapshot,
        _: &AiRequest,
    ) -> Result<String, PluginError> {
        check_channel(channel, provider)?;
        if operation != Operation::Infer {
            return Err(unsupported(operation));
        }
        Ok(PROTOCOL.into())
    }

    fn execute(
        host: &GuestHost,
        operation: Operation,
        channel: &str,
        input: OperationInput,
    ) -> Result<OperationOutput, PluginError> {
        if input.operation() != operation {
            return Err(plugin_error(
                ErrorKind::Invalid,
                "operation kind does not match input",
            ));
        }
        check_channel(channel, input.provider())?;
        match input {
            OperationInput::Allowance { provider, .. } => {
                allowance::execute(host, &provider).map(OperationOutput::Allowance)
            }
            OperationInput::Discover { provider, request } => {
                discovery::page(host, &provider, request.cursor.as_deref())
                    .map(OperationOutput::Discover)
            }
            OperationInput::Infer { provider, request } => infer(host, &provider, &request),
            _ => Err(unsupported(operation)),
        }
    }
}

#[cfg(target_arch = "wasm32")]
stravia_vendor_sdk::export_vendor!(Kiro);

fn check_channel(channel: &str, provider: &ProviderSnapshot) -> Result<(), PluginError> {
    if channel == CHANNEL_ID && provider.channel == channel && provider.provider_id == VENDOR_ID {
        Ok(())
    } else {
        Err(plugin_error(
            ErrorKind::Unsupported,
            "unsupported Kiro provider or channel",
        ))
    }
}

fn unsupported(op: Operation) -> PluginError {
    plugin_error(
        ErrorKind::Unsupported,
        format!("Kiro does not implement {}", op.as_str()),
    )
}

fn scoped_uuid(provider: &ProviderSnapshot, field: &str) -> Result<Option<Uuid>, PluginError> {
    provider
        .operation_metadata
        .get(field)
        .map(|v| {
            v.as_str()
                .and_then(|id| Uuid::try_parse(id).ok())
                .ok_or_else(|| plugin_error(ErrorKind::Invalid, format!("invalid host {field}")))
        })
        .transpose()
}

fn infer(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    request: &AiRequest,
) -> Result<OperationOutput, PluginError> {
    let model = provider.model.as_deref().unwrap_or(&request.model);
    // KAS 对 ACP 会话使用 sess_UUID；没有对话上下文时原生回退为独立 UUID。
    let conversation = match scoped_uuid(provider, "client_conversation_id")? {
        Some(id) => format!("sess_{id}"),
        None => Uuid::new_v4().to_string(),
    };
    let mut body = request::encode(request, model, &conversation)?;
    if let Some(turn) = scoped_uuid(provider, "user_prompt_id")? {
        body["conversationState"]["agentContinuationId"] = Value::String(turn.to_string());
    }
    let request = transport::rpc(provider, transport::Action::Generate, &body)?;
    host.emit_started()?;
    let response = transport::send(host, request)?;
    let mut parser = stream::KiroStream::new();
    let mut output = StreamResponseAccumulator::default();
    output.model = model.into();
    let mut emit = |delta: AiStreamDelta| {
        host.emit_delta(&delta)?;
        if let AiStreamDelta::StreamError { error } = delta {
            return Err(PluginError {
                kind: ErrorKind::upstream(Some(error.kind), None),
                message: error.message,
                upstream_status: error.status_code,
            });
        }
        output.apply(&delta);
        Ok(())
    };
    while let Some(chunk) = response.read_body()? {
        parser.feed(&chunk, &mut emit)?;
    }
    parser.finish(&mut emit)?;
    let complete = output.into_ai_response();
    host.emit_completed(&complete)?;
    Ok(OperationOutput::Infer(Box::new(complete)))
}
