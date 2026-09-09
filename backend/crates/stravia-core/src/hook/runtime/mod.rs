use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;

use super::tool::PlatformToolRegistry;
use futures::FutureExt;
use stravia_runtime_contract::hook::stream::is_semantic;
use stravia_runtime_contract::hook::*;
use stravia_runtime_contract::protocol::ir::{AiRequest, AiResponse, AiStreamDelta, ToolSpec};

mod types;
pub use types::*;

mod apply;
mod runtime;

use apply::*;
pub(crate) use runtime::{DetachedPlatformExecution, InferenceRun};
#[cfg(test)]
use runtime::{preserve_stream_coordinates, semantic_variant};

#[cfg(test)]
mod tests;
