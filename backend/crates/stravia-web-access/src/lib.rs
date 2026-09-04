//! Local、Exa 与 Zhipu Web Provider 的 Web Access 适配器实现。

mod browser;
pub mod fetch;
pub mod local;
mod outbound;
pub mod remote;
pub mod search;

pub use outbound::parse_cli_proxy;
pub use outbound::{LocalWeb, LocalWebError, OutboundProxyMode};
pub use stravia_web_access_contract::*;
