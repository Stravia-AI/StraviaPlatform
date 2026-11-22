//! 公共 Web 资源读取、重定向解析与远程 MCP HTTP 传输工具。
//!
//! 具体的搜索引擎与网页抓取 Provider 适配器已拆离出开源核心；外层宿主通过
//! [`stravia_web_access_contract::WebProviderFactory`] 注册实现。

pub mod fetch;
pub mod mcp_http;
pub mod wire;

pub use mcp_http::mcp_http_transport;
pub use stravia_web_access_contract::*;
