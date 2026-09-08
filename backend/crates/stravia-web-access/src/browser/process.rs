use super::egress::EgressProxy;
use anyhow::Context;
use std::{env, path::PathBuf, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    sync::oneshot,
};

pub(super) struct Process {
    pub(super) endpoint: String,
    shutdown: Option<oneshot::Sender<()>>,
    #[cfg(test)]
    pub(super) profile: PathBuf,
}

impl Drop for Process {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

async fn executable() -> anyhow::Result<PathBuf> {
    if let Some(path) = env::var_os("STRAVIA_CHROME_PATH") {
        let path = PathBuf::from(path);
        anyhow::ensure!(
            tokio::fs::metadata(&path).await?.is_file(),
            "STRAVIA_CHROME_PATH is not a file"
        );
        return Ok(path);
    }
    let mut candidates = Vec::new();
    if cfg!(target_os = "windows") {
        for root in ["PROGRAMFILES", "PROGRAMFILES(X86)", "LOCALAPPDATA"] {
            if let Some(root) = env::var_os(root) {
                for relative in [
                    "Google/Chrome/Application/chrome.exe",
                    "Chromium/Application/chrome.exe",
                ] {
                    candidates.push(PathBuf::from(&root).join(relative));
                }
            }
        }
    } else if cfg!(target_os = "macos") {
        candidates.extend(["/Applications/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing", "/Applications/Chromium.app/Contents/MacOS/Chromium", "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"].map(PathBuf::from));
    } else if let Some(path) = env::var_os("PATH") {
        for root in env::split_paths(&path) {
            for name in [
                "google-chrome-stable",
                "google-chrome",
                "chromium",
                "chromium-browser",
            ] {
                candidates.push(root.join(name));
            }
        }
    }
    for candidate in candidates {
        if tokio::fs::metadata(&candidate)
            .await
            .is_ok_and(|meta| meta.is_file())
        {
            return Ok(candidate);
        }
    }
    anyhow::bail!(
        "Chrome/Chromium not found; install it or set STRAVIA_CHROME_PATH (no automatic download)"
    )
}

impl Process {
    pub(super) async fn launch(proxy: EgressProxy) -> anyhow::Result<Self> {
        let executable = executable().await?;
        let profile = tokio::task::spawn_blocking(|| {
            tempfile::Builder::new().prefix("stravia-chrome-").tempdir()
        })
        .await??;
        let profile_path = profile.path().to_owned();
        let mut command = Command::new(executable);
        command
            .args([
                "--headless=new",
                "--disable-blink-features=AutomationControlled",
                "--window-size=1365,768",
                "--force-device-scale-factor=1.25",
                "--remote-debugging-address=127.0.0.1",
                "--remote-debugging-port=0",
                "--proxy-bypass-list=<-loopback>",
                "--disable-quic",
                "--force-webrtc-ip-handling-policy=disable_non_proxied_udp",
                "--no-first-run",
                "--no-default-browser-check",
                "--no-startup-window",
            ])
            .arg(format!("--proxy-server=http://{}", proxy.address()))
            .arg(format!("--user-data-dir={}", profile_path.display()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn()?;
        let mut stderr =
            BufReader::new(child.stderr.take().context("Chrome stderr unavailable")?).lines();
        let (ready_tx, ready) = oneshot::channel();
        let stderr_task = tokio::spawn(async move {
            let mut ready_tx = Some(ready_tx);
            let mut diagnostics = String::new();
            loop {
                match stderr.next_line().await {
                    Ok(Some(line)) => {
                        if let Some(endpoint) = line.strip_prefix("DevTools listening on ") {
                            if let Some(send) = ready_tx.take() {
                                let _ = send.send(Ok(endpoint.to_owned()));
                            }
                        } else if ready_tx.is_some() && diagnostics.len() + line.len() < 8192 {
                            diagnostics.push_str(&line);
                            diagnostics.push('\n');
                        }
                    }
                    result => {
                        if let Some(send) = ready_tx.take() {
                            let _ = send.send(Err(format!(
                                "Chrome exited before opening CDP: {result:?}\n{diagnostics}"
                            )));
                        }
                        break;
                    }
                }
            }
        });
        let (shutdown, stop) = oneshot::channel();
        tokio::spawn(async move {
            tokio::select! {
                _ = stop => {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                }
                _ = child.wait() => {}
            }
            stderr_task.abort();
            proxy.shutdown().await;
            // Windows 子进程可能短暂持有 profile 文件，退出后异步重试清理。
            let path = profile.keep();
            for attempt in 0..40 {
                match tokio::fs::remove_dir_all(&path).await {
                    Ok(()) => return,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
                    Err(error) if attempt == 39 => {
                        tracing::warn!(%error, path = %path.display(), "Chrome profile cleanup failed")
                    }
                    Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
                }
            }
        });
        let mut process = Self {
            endpoint: String::new(),
            shutdown: Some(shutdown),
            #[cfg(test)]
            profile: profile_path.clone(),
        };
        let endpoint = tokio::time::timeout(Duration::from_secs(20), ready)
            .await
            .context("Chrome CDP startup timed out")??
            .map_err(anyhow::Error::msg)?;
        process.endpoint = endpoint;
        Ok(process)
    }
}
