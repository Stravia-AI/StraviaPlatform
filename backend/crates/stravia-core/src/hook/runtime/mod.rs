use std::collections::{HashMap, HashSet};
use std::fmt;
use std::future::Future;
use std::sync::Arc;

use async_trait::async_trait;
use futures::FutureExt;

use crate::protocol::ids::ProtocolId;
use crate::protocol::ir::request::EmbeddingInput;
use crate::protocol::ir::{
    AiItem, AiRequest, AiResponse, AiStreamDelta, GenerationConfig, ProtocolExt, ToolCall,
    ToolChoice, ToolSpec,
};

use super::context::{ContextCompleteness, ContextSnapshot, ReplaceContextSpan};
use super::stream::{StreamDirective, StreamTransformer, is_semantic};
use super::tool::{PlatformToolRegistry, PlatformToolResult, ToolExecutionContext, ToolId};

mod types;
use types::SemanticVariant;
pub use types::*;

mod apply;
mod runtime;

use apply::*;
pub(crate) use runtime::{DetachedPlatformExecution, InferenceRun};
#[cfg(test)]
use runtime::{preserve_stream_coordinates, semantic_variant};

#[cfg(test)]
mod tests;
