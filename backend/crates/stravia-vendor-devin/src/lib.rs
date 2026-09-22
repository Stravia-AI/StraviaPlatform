mod allowance;
mod codec;
mod devin;

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
