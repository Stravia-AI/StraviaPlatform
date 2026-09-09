//! 跨能力 crate 共用的规范化语义与宿主执行契约。

pub mod agent;
pub mod artifact;
mod cancellation;
pub mod hook;
mod identity;
pub mod model_turn;
pub mod protocol;
pub mod redaction;
pub mod thinking;
pub mod turn_chain;

pub use cancellation::CancellationToken;
pub use identity::Principal;
