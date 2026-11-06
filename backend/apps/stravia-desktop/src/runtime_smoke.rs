use stravia_core::{Gateway, config::GatewayConfig, plugin::PluginSource};

pub(super) fn run() -> anyhow::Result<()> {
    anyhow::ensure!(
        std::env::args_os().len() == 2,
        "--smoke-test-runtime does not accept other arguments"
    );
    // 新目录保证不会用准入记录跳过 guest 执行，也不会触碰用户实例。
    let directory = tempfile::Builder::new()
        .prefix("stravia-runtime-smoke-")
        .tempdir()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(async {
        let gateway = Gateway::new(GatewayConfig {
            data_dir: directory.path().to_path_buf(),
            catalog_base_url: None,
            catalog_background_refresh: false,
            // 即使未来启动流程增加更新检查，也不能连接生产分发服务。
            update_source_url: Some(String::new()),
            ..Default::default()
        })
        .await?;
        let plugins = gateway.admin().list_vendor_plugins().await;
        gateway.shutdown().await;
        anyhow::ensure!(
            plugins?.iter().any(|plugin| {
                plugin.vendor_id == "base"
                    && plugin.source == PluginSource::Builtin
                    && plugin.error.is_none()
            }),
            "bundled Base plugin did not load successfully"
        );
        Ok::<(), anyhow::Error>(())
    });
    drop(runtime);
    directory.close()?;
    result?;
    println!("Desktop runtime smoke passed: bundled Base Wasm executed");
    Ok(())
}
