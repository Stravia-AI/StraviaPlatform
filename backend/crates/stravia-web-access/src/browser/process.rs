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

/// 检查手动指定的绝对路径是否为可执行文件，不启动程序，也不验证浏览器版本。
/// 文件不存在、不可访问、不是普通文件或不符合平台可执行条件时返回错误。
pub async fn validate_browser_executable(path: &std::path::Path) -> anyhow::Result<()> {
    anyhow::ensure!(
        path.is_absolute(),
        "browser executable path must be absolute"
    );
    let metadata = tokio::fs::metadata(path)
        .await
        .with_context(|| format!("cannot access browser executable: {}", path.display()))?;
    anyhow::ensure!(metadata.is_file(), "browser executable path is not a file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        anyhow::ensure!(
            metadata.permissions().mode() & 0o111 != 0,
            "browser file is not executable"
        );
    }
    #[cfg(windows)]
    anyhow::ensure!(
        path.extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("exe")),
        "browser executable must be an .exe file"
    );
    Ok(())
}

/// 按手动路径、环境变量、本机安装位置的顺序解析浏览器，不启动或下载程序。
/// 显式路径无效时返回错误，不回退；环境变量中的相对路径仍按工作目录解释。
pub async fn resolve_browser_executable(
    manual_path: Option<&std::path::Path>,
) -> anyhow::Result<PathBuf> {
    if let Some(path) = manual_path {
        validate_browser_executable(path).await?;
        return Ok(path.to_owned());
    }
    if let Some(path) = env::var_os("STRAVIA_CHROME_PATH") {
        let path = std::path::absolute(path)?;
        validate_browser_executable(&path)
            .await
            .context("invalid STRAVIA_CHROME_PATH")?;
        return Ok(path);
    }
    let mut candidates = Vec::new();
    if cfg!(target_os = "windows") {
        for root in [
            "ProgramW6432",
            "PROGRAMFILES",
            "PROGRAMFILES(X86)",
            "LOCALAPPDATA",
        ] {
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
        if let Some(home) = env::var_os("HOME") {
            for relative in [
                "Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
                "Applications/Chromium.app/Contents/MacOS/Chromium",
                "Applications/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
            ] {
                candidates.push(PathBuf::from(&home).join(relative));
            }
        }
    } else {
        let path = env::var_os("PATH").unwrap_or_default();
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
    if cfg!(target_os = "linux") {
        candidates.extend(
            [
                "/usr/bin/google-chrome",
                "/usr/bin/chromium",
                "/usr/bin/chromium-browser",
                "/snap/bin/chromium",
            ]
            .map(PathBuf::from),
        );
    }
    for candidate in candidates {
        let candidate = if candidate.is_absolute() {
            candidate
        } else {
            std::path::absolute(candidate)?
        };
        if validate_browser_executable(&candidate).await.is_ok() {
            return Ok(candidate);
        }
    }
    anyhow::bail!(
        "Chrome/Chromium not found; install it or set STRAVIA_CHROME_PATH (no automatic download)"
    )
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn invalid_explicit_browser_never_falls_back() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("removed-chrome.exe");
        let error = super::resolve_browser_executable(Some(&missing))
            .await
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::NotFound
        );
        assert!(super::resolve_browser_executable(Some(directory.path()))
            .await
            .is_err());
        assert!(
            super::resolve_browser_executable(Some(std::path::Path::new("chrome.exe")))
                .await
                .is_err()
        );
    }
}

impl Process {
    pub(super) async fn launch(
        proxy: EgressProxy,
        browser_path: Option<&std::path::Path>,
    ) -> anyhow::Result<Self> {
        let executable = resolve_browser_executable(browser_path).await?;
        let profile = tokio::task::spawn_blocking(|| {
            tempfile::Builder::new().prefix("stravia-chrome-").tempdir()
        })
        .await??;
        let profile_path = profile.path().to_owned();
        #[cfg(test)]
        super::tests::configure_http_fixture_profile(&profile_path).await?;
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
