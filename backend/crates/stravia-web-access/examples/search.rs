use stravia_web_access::{
    parse_cli_proxy, search::engines::ProgressUpdateData, LocalWeb, OutboundProxyMode,
};

#[tokio::main]
async fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt::init();

    let mut args = std::env::args().skip(1);
    let (mode, query) = match args.next().as_deref() {
        Some("--proxy") => {
            let proxy = args.next().ok_or_else(|| {
                eyre::eyre!("usage: search [--proxy direct|system|<url>] <query>")
            })?;
            let query = args.next().ok_or_else(|| {
                eyre::eyre!("usage: search [--proxy direct|system|<url>] <query>")
            })?;
            (parse_cli_proxy(&proxy)?, query)
        }
        Some(query) => (OutboundProxyMode::Direct, query.to_string()),
        None => eyre::bail!("usage: search [--proxy direct|system|<url>] <query>"),
    };

    let web = LocalWeb::new(mode)?;
    let search = web.search_query(query);
    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
    let search_future = tokio::spawn(async move { web.search(search, progress_tx).await });

    while let Some(progress_update) = progress_rx.recv().await {
        match progress_update.data {
            ProgressUpdateData::Engine { engine, update } => {
                tracing::info!(%engine, ?update, time_ms = progress_update.time_ms, "engine progress");
            }
            ProgressUpdateData::Response(response) => {
                println!("{}", serde_json::to_string_pretty(&response)?);
            }
            ProgressUpdateData::PostSearchInfobox(infobox) => {
                tracing::info!(engine = %infobox.engine, time_ms = progress_update.time_ms, "postsearch infobox");
            }
        }
    }

    search_future.await??;
    Ok(())
}
