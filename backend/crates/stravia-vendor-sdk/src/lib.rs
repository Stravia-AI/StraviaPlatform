//! Guest-side SDK for Stravia vendor Wasm plugins.
//!
//! A plugin crate links this SDK, fills a [`VendorDescriptor`], implements
//! [`VendorGuest`] for the capabilities it claims, and calls
//! [`export_vendor!`] once. The generated component exposes the versioned
//! `stravia:vendor@0.2.0` world; all model traffic flows through the canonical
//! contract in [`stravia_runtime_contract::protocol::ir`].
//!
//! The WIT contract lives in `wit/` of this crate and is the single source for
//! both guest bindings (here) and host bindings (`stravia-vendor-runtime`).

#[doc(hidden)]
pub mod bindings {
    wit_bindgen::generate!({
        world: "vendor",
        path: "wit",
        generate_all,
        pub_export_macro: true,
    });
}

mod descriptor;
mod envelope;
#[doc(hidden)]
pub mod guest;
mod host_api;

pub use crate::bindings::stravia::vendor::host::LogLevel;
pub use crate::bindings::stravia::vendor::types::{HttpRequest, WsRequest};
pub use descriptor::{
    AuthCallback, AuthCallbackPort, AuthDescriptor, AuthFlow, AuthManualInput, AuthManualInputType,
    Capability, ChannelDescriptor, ConfigField, ConfigFieldKind, DataCompatibility,
    DefaultModelsSource, EnumOption, FieldCondition, MODEL_CAPABILITY_THINKING_TOGGLE,
    MODELS_SOURCE_CATALOG, NetworkDeclaration, OriginDeclaration, ProviderDescriptor,
    VendorDescriptor, VendorKind,
};
pub use envelope::{CANONICAL_FORMAT_VERSION, CanonicalEnvelope, decode_payload, encode_payload};
pub use guest::{
    AllowanceAmount, AllowanceItem, AllowanceRequest, AllowanceResponse, AuthRequest, AuthResponse,
    AuthStep, ConfigValidationRequest, ConfigValidationResponse, DiscoverRequest, DiscoverResponse,
    DiscoveredModel, ErrorKind, MediaArtifact, MediaImageAspectRatio, MediaImageRequest,
    MediaImageResolution, MediaImageResponse, MediaReference, ModelAllowance, ModelErrorKind,
    ModelMetadata, Operation, OperationInput, OperationOutput, PluginError, ProviderSnapshot,
    SearchRequest, SearchResponse, SearchSource, TRANSPORT_PREFERENCE_METADATA_KEY,
    TransportFailure, TransportPreference, UpstreamFailure, ValidationIssue, VendorGuest,
};
pub use host_api::{GuestHost, HttpResponse, WsConnection, WsMessage, read_http_body};
pub use stravia_runtime_contract::protocol::ir::{
    AiError, AiErrorKind, AiRequest, AiResponse, AiStreamDelta, NativeCompactionResponse,
};

/// Generated WIT bindings. Plugin code normally uses [`VendorGuest`] and the
/// re-exported helpers instead of touching this module directly.
pub mod wit {
    pub use crate::bindings::stravia::vendor::host;
    pub use crate::bindings::stravia::vendor::types;
    pub use crate::bindings::{Guest, export};
}
