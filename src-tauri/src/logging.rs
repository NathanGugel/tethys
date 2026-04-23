use std::path::Path;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

/// Initialize global tracing: stderr for dev, rolling daily file in `logs_dir`.
/// Returned guard must live for the duration of the app — dropping it flushes.
pub fn init(logs_dir: &Path) -> WorkerGuard {
    let file_appender = tracing_appender::rolling::daily(logs_dir, "tethys.log");
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,tethys_lib=debug"));

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(std::io::stderr).with_ansi(true))
        .with(
            fmt::layer()
                .with_writer(file_writer)
                .with_ansi(false)
                .with_target(true),
        )
        .init();

    guard
}
