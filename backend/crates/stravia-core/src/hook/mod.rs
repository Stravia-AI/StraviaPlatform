mod runtime;
pub mod stream;
pub mod tool;

pub mod context;

pub use context::{
    ContextCheckpoint, ContextCompleteness, ContextItem, ContextItemId, ContextPatchError,
    ContextSnapshot, OpaqueContextRef, ReplaceContextSpan,
};
pub use runtime::{
    ActionBatch, ClassifiedToolCalls, EventKind, Hook, HookAction, HookControl, HookDescriptor,
    HookError, HookEvent, HookId, HookRejection, HookRuntime, HookSession, PlatformToolCall,
    Principal, RequestKind, RequestPatch, ResponsePatch, RouteContext, SessionContext,
    ToolResultPatch, TransportKind,
};
pub(crate) use runtime::{DetachedPlatformExecution, InferenceRun, ResponseHookOutcome};
pub use stream::{StreamDirective, StreamTransformer};
pub use tool::{
    ExposedPlatformTool, PlatformTool, PlatformToolError, PlatformToolOutput, PlatformToolRegistry,
    PlatformToolResult, ToolExecutionContext, ToolId, ToolProgress, ToolProgressSink,
};

pub(crate) fn is_secret_key(key: &str) -> bool {
    let normalized = key.to_ascii_lowercase().replace(['-', '.'], "_");
    normalized == "authorization"
        || normalized.contains("authorization")
        || normalized == "proxy_authorization"
        || normalized == "api_key"
        || normalized == "apikey"
        || normalized == "token"
        || normalized == "refresh_token"
        || normalized.ends_with("_token")
        || normalized.starts_with("token_")
        || normalized == "secret"
        || normalized.contains("secret")
        || normalized == "password"
        || normalized.contains("password")
        || normalized == "credential"
        || normalized == "credentials"
}

pub(crate) fn redact_vendor_value(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(object) => {
            let mut redacted = serde_json::Map::new();
            for (key, value) in object {
                if !is_secret_key(key) {
                    redacted.insert(key.clone(), redact_vendor_value(value));
                }
            }
            serde_json::Value::Object(redacted)
        }
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.iter().map(redact_vendor_value).collect())
        }
        _ => value.clone(),
    }
}
