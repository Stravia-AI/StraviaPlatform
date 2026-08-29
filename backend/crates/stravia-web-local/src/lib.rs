//! Local Web Provider 的进程内 search 与 fetch 实现；HTTP UI/API 已移除。

mod browser;
pub mod fetch;
mod outbound;
pub mod search;

pub use outbound::parse_cli_proxy;
pub use outbound::{LocalWeb, LocalWebError, OutboundProxyMode};
