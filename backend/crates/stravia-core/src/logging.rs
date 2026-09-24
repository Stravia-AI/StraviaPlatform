//! 进程级 tracing 初始化：在 stdout 之外把同一过滤级别的输出落到按天滚动的日志文件。
//! 打包桌面进程没有可读的 stdout，服务端也常脱离终端运行，文件是唯一可回看的渠道。

use std::path::PathBuf;

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, fmt};

/// 日志目录保留的滚动文件数上限；按天滚动下单文件不封顶，
/// 以天数约束目录占用换取稳定可预期的命名。
const MAX_LOG_FILES: usize = 7;

/// 非阻塞写线程的存活凭证；提前 drop 会停掉写线程并丢弃缓冲日志。
/// 进程持有它直到退出即可。
pub struct RuntimeLoggingGuard {
    _worker: Option<tracing_appender::non_blocking::WorkerGuard>,
}

/// 安装全局 tracing subscriber：stdout 与 `log_dir` 下按天滚动的文件同时输出。
///
/// `log_dir` 不可用或日志文件创建失败时退回 stdout-only，并向 stderr 说明原因；
/// 全局 subscriber 已安装（例如桌面第二实例仍执行到 setup）时保持原状。
/// 返回的 guard 必须由调用方持有到进程结束。
pub fn init_runtime_logging(
    filter: &str,
    log_dir: Option<PathBuf>,
    file_prefix: &str,
) -> RuntimeLoggingGuard {
    let filter = EnvFilter::new(filter);
    let stdout_layer = fmt::layer().with_filter(filter.clone());
    let mut guard = RuntimeLoggingGuard { _worker: None };
    let file_layer = log_dir.and_then(|dir| match build_file_writer(&dir, file_prefix) {
        Ok((writer, worker)) => {
            guard._worker = Some(worker);
            Some(
                fmt::layer()
                    .with_ansi(false)
                    .with_writer(writer)
                    .with_filter(filter),
            )
        }
        Err(error) => {
            eprintln!(
                "Stravia file logging unavailable under {}: {error}",
                dir.display()
            );
            None
        }
    });
    let registry = tracing_subscriber::registry().with(stdout_layer);
    let installed = match file_layer {
        Some(layer) => registry.with(layer).try_init().is_ok(),
        None => registry.try_init().is_ok(),
    };
    if !installed {
        // 未接管全局 subscriber 时留着写线程只会白转；调用方拿不到有效输出。
        guard._worker = None;
    }
    guard
}

fn build_file_writer(
    dir: &std::path::Path,
    prefix: &str,
) -> anyhow::Result<(
    tracing_appender::non_blocking::NonBlocking,
    tracing_appender::non_blocking::WorkerGuard,
)> {
    std::fs::create_dir_all(dir)?;
    let appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix(prefix)
        .filename_suffix("log")
        .max_log_files(MAX_LOG_FILES)
        .build(dir)?;
    Ok(tracing_appender::non_blocking(appender))
}
