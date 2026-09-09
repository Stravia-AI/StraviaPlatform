mod runtime;
pub mod tool;

pub use runtime::HookRuntime;
pub(crate) use runtime::{DetachedPlatformExecution, InferenceRun};
pub use tool::PlatformToolRegistry;

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
