#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use clipbridge::{clipboard, config, logging};

fn main() -> anyhow::Result<()> {
    let path = config::default_config_path();
    let cfg = config::Config::load_or_create(&path)?;
    let _guard = logging::init(Some(&config::default_log_dir()), true, true)?;
    tracing::info!("config {} device {}", path.display(), cfg.device_name);
    let mut changes = clipboard::spawn_watcher()?;
    let handle = clipboard::ClipboardHandle::spawn(Box::new(clipboard::ArboardBackend::new()?))?;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        while changes.changed().await.is_ok() {
            let opts = clipboard::ReadOpts {
                want_images: true,
                known: None,
            };
            tracing::info!(
                "clipboard changed: {:?}",
                handle.read(opts).await.map(|o| match o {
                    clipboard::ReadOutcome::Clip { kind, .. } =>
                        format!("{} {}B", kind.label(), kind.byte_len()),
                    other => format!("{other:?}"),
                })
            );
        }
    });
    Ok(())
}
