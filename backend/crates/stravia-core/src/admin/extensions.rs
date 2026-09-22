use super::*;

impl AdminService {
    pub async fn list_vendor_plugins(&self) -> anyhow::Result<Vec<crate::plugin::PluginSummary>> {
        self.gw.vendor_plugins.list(&self.gw).await
    }

    pub async fn preview_vendor_plugin(
        &self,
        component: Vec<u8>,
    ) -> anyhow::Result<crate::plugin::PluginPreview> {
        self.gw
            .vendor_plugins
            .preview(
                &self.gw,
                component.into(),
                crate::plugin::PluginSource::Local,
                None,
            )
            .await
    }

    pub async fn confirm_vendor_plugin(
        &self,
        input: crate::plugin::ConfirmPluginUpdate,
    ) -> anyhow::Result<crate::plugin::PluginSummary> {
        self.gw.vendor_plugins.confirm(&self.gw, input).await
    }

    pub async fn preview_builtin_vendor_plugin(
        &self,
        vendor_id: &str,
    ) -> anyhow::Result<crate::plugin::PluginPreview> {
        self.gw.vendor_plugins.restore(&self.gw, vendor_id).await
    }

    /// 卸载指定 Vendor 的专属插件，即使当前组件无法加载也可执行。
    ///
    /// 取消并排空该 Vendor 的活动调用与认证会话，移除安装记录，但保留连接、
    /// 凭据、路由、历史、插件私有数据和产物文件。空 ID、`base`、未安装的插件，
    /// 以及任务排空或存储失败均返回错误；取消不能保证上游停止执行或计费。
    pub async fn uninstall_vendor_plugin(&self, vendor_id: &str) -> anyhow::Result<()> {
        self.gw.vendor_plugins.uninstall(&self.gw, vendor_id).await
    }

    /// 返回当前已安装且可加载的 Provider profile；未知或不可用的 supplier 返回错误。
    pub fn vendor_metadata(
        &self,
        provider_id: &str,
    ) -> anyhow::Result<stravia_vendor_sdk::ProviderDescriptor> {
        self.gw.vendor_plugins.descriptor(provider_id)
    }

    /// Effective provider profiles reported by installed and loadable Vendor Components.
    /// Dedicated packages replace the fallback profile as a whole.
    pub async fn list_vendor_metadata(
        &self,
    ) -> anyhow::Result<Vec<stravia_vendor_sdk::ProviderDescriptor>> {
        Ok(self.gw.vendor_plugins.descriptors())
    }

    /// Runtime inventory: loaded Vendor Components plus the protocol codec
    /// endpoints registered in this process. Only executable runtime entries
    /// are exposed as management metadata.
    pub async fn list_loaded_extensions(&self) -> anyhow::Result<Vec<Value>> {
        let mut extensions = Vec::new();
        for plugin in self.gw.vendor_plugins.list(&self.gw).await? {
            extensions.push(serde_json::json!({
                "id": plugin.vendor_id,
                "capability": "vendor_plugin",
                "version": plugin.version,
                "source": plugin.source,
                "status": plugin.status,
            }));
        }
        for endpoint in stravia_protocol_codec::registry::ProtocolRegistry::global().endpoints() {
            extensions.push(serde_json::json!({
                "id": endpoint.to_string(),
                "capability": "protocol_endpoint",
            }));
        }
        extensions.sort_by(|left, right| {
            left.get("id")
                .and_then(Value::as_str)
                .cmp(&right.get("id").and_then(Value::as_str))
        });
        Ok(extensions)
    }
}
