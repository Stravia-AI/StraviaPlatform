mod allowance;
mod codec;
mod devin;
mod messages {
    include!(concat!(env!("OUT_DIR"), "/messages.rs"));
}

use stravia_runtime_contract::protocol::ir::AiRequest;
#[cfg(target_arch = "wasm32")]
use stravia_vendor_sdk::VendorGuest;
use stravia_vendor_sdk::{
    ErrorKind, GuestHost, Operation, OperationInput, OperationOutput, PluginError,
    ProviderSnapshot, VendorDescriptor,
};

pub fn descriptor() -> VendorDescriptor {
    devin::descriptor()
}

pub fn select_protocol(
    operation: Operation,
    channel: &str,
    provider: &ProviderSnapshot,
    _request: &AiRequest,
) -> Result<String, PluginError> {
    validate_provider(channel, provider)?;
    if operation != Operation::Infer {
        return Err(error(
            ErrorKind::Unsupported,
            format!("Devin does not implement {}", operation.as_str()),
        ));
    }
    Ok("devin-connect".into())
}

pub fn check_connection(
    host: &GuestHost,
    channel: &str,
    provider: ProviderSnapshot,
) -> Result<stravia_vendor_sdk::ConnectionCheckResponse, PluginError> {
    validate_provider(channel, &provider)?;
    allowance::check(host, &provider)
}

pub fn execute(
    host: &GuestHost,
    operation: Operation,
    channel: &str,
    input: OperationInput,
) -> Result<OperationOutput, PluginError> {
    if input.operation() != operation {
        return Err(error(
            ErrorKind::Invalid,
            "operation kind does not match typed operation input",
        ));
    }
    devin::execute(host, operation, channel, input)
}

fn validate_provider(channel: &str, provider: &ProviderSnapshot) -> Result<(), PluginError> {
    if provider.provider_id != "devin" || channel != "devin" || provider.channel != channel {
        return Err(error(
            ErrorKind::Unsupported,
            "unsupported Devin provider or channel",
        ));
    }
    Ok(())
}

fn error(kind: ErrorKind, message: impl Into<String>) -> PluginError {
    PluginError {
        kind,
        message: message.into(),
        upstream_status: None,
    }
}

#[cfg(target_arch = "wasm32")]
struct DevinVendor;

#[cfg(target_arch = "wasm32")]
impl VendorGuest for DevinVendor {
    fn descriptor() -> VendorDescriptor {
        crate::descriptor()
    }

    fn check_connection(
        host: &GuestHost,
        channel: &str,
        provider: ProviderSnapshot,
    ) -> Result<stravia_vendor_sdk::ConnectionCheckResponse, PluginError> {
        crate::check_connection(host, channel, provider)
    }

    fn select_protocol(
        operation: Operation,
        channel: &str,
        provider: &ProviderSnapshot,
        request: &AiRequest,
    ) -> Result<String, PluginError> {
        crate::select_protocol(operation, channel, provider, request)
    }

    fn execute(
        host: &GuestHost,
        operation: Operation,
        channel: &str,
        input: OperationInput,
    ) -> Result<OperationOutput, PluginError> {
        crate::execute(host, operation, channel, input)
    }
}

#[cfg(target_arch = "wasm32")]
stravia_vendor_sdk::export_vendor!(DevinVendor);

fn net_fields(headers: Vec<(String, String)>) -> stravia_vendor_sdk::net::Fields {
    let mut fields = stravia_vendor_sdk::net::Fields::new();
    for (name, value) in headers {
        fields.append(name, value.into_bytes());
    }
    if fields.get("accept").is_none() {
        fields.append("accept", b"*/*".to_vec());
    }
    if fields.get("accept-encoding").is_none() {
        fields.append("accept-encoding", b"identity".to_vec());
    }
    if fields.get("user-agent").is_none() {
        fields.append("user-agent", b"Stravia".to_vec());
    }
    fields
}

fn fingerprint(provider: &ProviderSnapshot, token: &str) -> Result<String, PluginError> {
    let id = provider
        .operation_metadata
        .get("client_installation_id")
        .and_then(serde_json::Value::as_str)
        .and_then(|value| uuid::Uuid::parse_str(value).ok())
        .ok_or_else(|| {
            error(
                ErrorKind::Invalid,
                "Devin requires a valid client_installation_id",
            )
        })?;
    codec::devin_connect::request::device_fingerprint(token, id).map_err(|err| {
        error(
            ErrorKind::Invalid,
            format!("cannot create Devin fingerprint: {err}"),
        )
    })
}

fn inference_fields(headers: Vec<(String, String)>) -> stravia_vendor_sdk::net::Fields {
    let mut fields = stravia_vendor_sdk::net::Fields::new();
    for (name, value) in headers {
        fields.append(name, value.into_bytes());
    }
    fields
}

fn inference_options() -> stravia_vendor_sdk::net::Options {
    let mut options = stravia_vendor_sdk::net::Options::new();
    options.shape("devin").expect("devin shape is valid");
    options
}

fn net_options() -> stravia_vendor_sdk::net::Options {
    let mut options = stravia_vendor_sdk::net::Options::new();
    options.shape("default").expect("default shape is valid");
    options
}
