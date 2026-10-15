#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clipbridge::app::{self, AppEvent, AppOptions, UiCmd};
use clipbridge::{clipboard, config, control, gui, logging, tray};
use tokio::sync::mpsc;

const USAGE: &str = "\
ClipBridge: LAN clipboard sync

USAGE: clipbridge [OPTIONS]

OPTIONS:
  --config <path>  Use this config file instead of the default location
  --headless       No tray icon; log to stderr; stop with Ctrl+C
  --verbose        Debug logging
  --settings       Open the settings window (talks to the running instance)
  -h, --help       Show this help
";

struct Args {
    config: Option<PathBuf>,
    headless: bool,
    verbose: bool,
    settings: bool,
}

fn parse_args() -> Result<Args> {
    let mut args = Args {
        config: None,
        headless: false,
        verbose: false,
        settings: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--config" => args.config = Some(it.next().context("--config needs a path")?.into()),
            "--headless" => args.headless = true,
            "--verbose" | "-v" => args.verbose = true,
            "--settings" => args.settings = true,
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            other => bail!("unknown argument: {other}\n\n{USAGE}"),
        }
    }
    Ok(args)
}

fn main() {
    if let Err(e) = real_main() {
        // In release builds on Windows there is no console; the log file is the record.
        tracing::error!("fatal: {e:#}");
        eprintln!("clipbridge: {e:#}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let args = parse_args()?;
    let config_path = args
        .config
        .clone()
        .unwrap_or_else(config::default_config_path);
    // Forwarded to the settings window, autostart entry, etc.
    let passthrough: Vec<String> = match &args.config {
        Some(p) => vec!["--config".into(), p.display().to_string()],
        None => Vec::new(),
    };

    if args.settings {
        let _ = logging::init(None, args.verbose, true);
        return gui::run(config_path, passthrough);
    }

    // Single instance: if one is already running, ask it to show the settings window.
    if control::request(&config_path, control::Command::OpenSettings).is_ok() {
        eprintln!("ClipBridge is already running; opened its settings window.");
        return Ok(());
    }

    let log_dir = config::default_log_dir();
    let to_stderr = args.headless || args.verbose || cfg!(debug_assertions);
    let _log_guard = logging::init(Some(&log_dir), args.verbose, to_stderr)?;
    tracing::info!(
        "ClipBridge {} starting; config {}",
        env!("CARGO_PKG_VERSION"),
        config_path.display()
    );
    // Create the config early so a first-run user can find and edit it.
    if let Err(e) = config::Config::load_or_create(&config_path) {
        tracing::error!("{e:#}");
    }

    let clipboard = clipboard::spawn_watcher()
        .and_then(|changes| {
            let backend = clipboard::ArboardBackend::new()?;
            Ok((
                clipboard::ClipboardHandle::spawn(Box::new(backend))?,
                changes,
            ))
        })
        .map_err(|e| {
            tracing::error!("clipboard unavailable: {e:#}");
            format!("{e:#}")
        });
    let opts = AppOptions {
        config_path: config_path.clone(),
        log_dir: log_dir.clone(),
        clipboard,
    };
    let (ui_tx, ui_rx) = mpsc::channel(16);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .context("start async runtime")?;

    if args.headless {
        runtime.block_on(async move {
            let quit = ui_tx.clone();
            tokio::spawn(async move {
                if tokio::signal::ctrl_c().await.is_ok() {
                    let _ = quit.send(UiCmd::Quit).await;
                }
            });
            // Log only when the one-line summary changes, not on every RTT update.
            let last = std::sync::Mutex::new(String::new());
            app::run(opts, ui_rx, move |ev| {
                if let AppEvent::Status(s) = ev
                    && let Ok(mut last) = last.lock()
                    && *last != s.summary
                {
                    tracing::info!("status: {}", s.summary);
                    *last = s.summary;
                }
            })
            .await;
        });
        return Ok(());
    }

    run_tray(
        runtime,
        opts,
        ui_tx,
        ui_rx,
        tray::TrayOptions {
            config_path,
            log_dir,
            passthrough_args: passthrough,
        },
    )
}

fn run_tray(
    runtime: tokio::runtime::Runtime,
    opts: AppOptions,
    ui_tx: mpsc::Sender<UiCmd>,
    ui_rx: mpsc::Receiver<UiCmd>,
    tray_opts: tray::TrayOptions,
) -> Result<()> {
    use tao::event_loop::EventLoopBuilder;

    #[allow(unused_mut)]
    let mut event_loop = EventLoopBuilder::<tray::UserEvent>::with_user_event().build();
    #[cfg(target_os = "macos")]
    {
        use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
        // Menu-bar only: no Dock icon, no app switcher entry.
        event_loop.set_activation_policy(ActivationPolicy::Accessory);
    }
    let proxy = event_loop.create_proxy();
    tray::install_event_handlers(&proxy);

    std::thread::Builder::new()
        .name("runtime".into())
        .spawn(move || {
            let sink_proxy = proxy.clone();
            runtime.block_on(app::run(opts, ui_rx, move |ev| {
                let _ = sink_proxy.send_event(tray::UserEvent::App(ev));
            }));
            runtime.shutdown_timeout(std::time::Duration::from_secs(2));
            let _ = proxy.send_event(tray::UserEvent::Exited);
        })
        .context("spawn runtime thread")?;

    tray::run(event_loop, ui_tx, tray_opts)
}
