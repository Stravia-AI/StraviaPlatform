//! Shared request, response, error, URL, and thinking utilities for Stravia vendor guests.

pub mod common;
pub mod thinking;

pub use common::{
    decode_ai_response, decode_compaction, decode_compaction_preserving_upstream_errors,
    decode_inference, emit_deltas, encode_inference_request, endpoint, endpoint_url, header_pairs,
    map_request_transform_error, map_response_transform_error, model_discovery_url, model_error,
    plugin_error, unsupported, upstream_error,
};
