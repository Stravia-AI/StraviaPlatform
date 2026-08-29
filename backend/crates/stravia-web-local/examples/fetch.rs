use stravia_web_local::{parse_cli_proxy, LocalWeb, OutboundProxyMode};

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let mut args = std::env::args().skip(1);
    let (mode, url) = match args.next().as_deref() {
        Some("--proxy") => {
            let proxy = args.next().ok_or_else(|| {
                eyre::eyre!("usage: fetch [--proxy direct|system|<url>] <public-http-url>")
            })?;
            let url = args.next().ok_or_else(|| {
                eyre::eyre!("usage: fetch [--proxy direct|system|<url>] <public-http-url>")
            })?;
            (parse_cli_proxy(&proxy)?, url)
        }
        Some(url) => (OutboundProxyMode::Direct, url.to_string()),
        None => eyre::bail!("usage: fetch [--proxy direct|system|<url>] <public-http-url>"),
    };
    let web = LocalWeb::new(mode)?;
    let page = web.fetch(&url).await?;
    println!("{}", serde_json::to_string_pretty(&page)?);
    Ok(())
}
