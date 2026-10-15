//! Logging: daily-rolling file in the platform data dir, plus stderr when useful.

use std::path::Path;

use anyhow::{Context, Result};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

pub const LOG_FILE_PREFIX: &str = "clipbridge";

/// Initializes the global subscriber. Keep the returned guard alive until exit,
/// otherwise buffered log lines are lost.
pub fn init(log_dir: Option<&Path>, verbose: bool, to_stderr: bool) -> Result<Option<WorkerGuard>> {
    let default = if verbose {
        "clipbridge=debug,info"
    } else {
        "clipbridge=info,warn"
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));

    let (file_layer, guard) = match log_dir {
        Some(dir) => {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
            let appender = tracing_appender::rolling::Builder::new()
                .rotation(tracing_appender::rolling::Rotation::DAILY)
                .filename_prefix(LOG_FILE_PREFIX)
                .filename_suffix("log")
                .max_log_files(7)
                .build(dir)
                .context("create log file")?;
            let (writer, guard) = tracing_appender::non_blocking(appender);
            let layer = fmt::layer()
                .with_ansi(false)
                .with_target(false)
                .with_writer(writer);
            (Some(layer), Some(guard))
        }
        None => (None, None),
    };
    let stderr_layer =
        to_stderr.then(|| fmt::layer().with_target(false).with_writer(std::io::stderr));

    tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(stderr_layer)
        .try_init()
        .context("init logging")?;
    Ok(guard)
}

/// Newest log file in `dir`, for the settings window's log tail.
pub fn latest_log_file(dir: &Path) -> Option<std::path::PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with(LOG_FILE_PREFIX))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .max_by_key(|(t, _)| *t)
        .map(|(_, p)| p)
}
