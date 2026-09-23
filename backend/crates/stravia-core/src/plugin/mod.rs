//! Wasm Vendor 组件的安装、生命周期与受控宿主能力。
//!
//! Vendor 清单以已安装组件为准；协议 codec 注册表属于通用协议层，不能再与
//! 编译期 Vendor inventory 拼成一套伪插件清单。

mod artifacts;
mod builtin;
pub mod catalog_sync;
pub(crate) mod execution;
mod lifecycle;
pub(crate) mod manager;
pub(crate) mod network;
pub(crate) mod permissions;
mod store;
mod types;

pub(crate) use execution::{
    VendorCallContext, VendorEvent, VendorExecution, VendorRequest, VendorSessionScope,
};
pub(crate) use lifecycle::VendorPublicationFence;
pub use store::PluginStore;
pub use types::{
    ConfirmPluginUpdate, PluginBindingImpact, PluginDataDiscard, PluginNetworkPermission,
    PluginPreview, PluginProvider, PluginSource, PluginSummary,
};
