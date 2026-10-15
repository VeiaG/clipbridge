//! App supervisor: owns the config, (re)starts the sync node, serves the control
//! channel and publishes status to the tray.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::clipboard::ClipboardHandle;
use crate::config::Config;
use crate::control::{self, Command, ControlRequest, Response};
use crate::crypto::Cipher;
use crate::net::{PeerStatus, PendingStatus};
use crate::node::{self, NodeHandle, NodeParams};
use crate::sync::SyncStats;

const RETRY_START: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Starting,
    NotConfigured,
    Active,
    Paused,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppStatus {
    pub state: State,
    /// One-line human summary (tray menu / tooltip).
    pub summary: String,
    pub error: Option<String>,
    pub device_name: String,
    pub device_id: String,
    pub listen_port: u16,
    pub peers: Vec<PeerStatus>,
    pub pending: Vec<PendingStatus>,
    pub stats: SyncStats,
    pub config_path: String,
    pub log_dir: String,
    pub version: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiCmd {
    SetPaused(bool),
    Quit,
}

#[derive(Debug, Clone)]
pub enum AppEvent {
    Status(Box<AppStatus>),
    /// Someone (a second launch) asked to show the settings window.
    OpenSettings,
}

pub struct AppOptions {
    pub config_path: PathBuf,
    pub log_dir: PathBuf,
    /// `Err` if the system clipboard could not be opened.
    pub clipboard: Result<(ClipboardHandle, watch::Receiver<u64>), String>,
}

struct App {
    opts: AppOptions,
    config: Option<Config>,
    config_mtime: Option<SystemTime>,
    cipher: Option<(String, Cipher)>,
    node: Option<NodeHandle>,
    paused: watch::Sender<bool>,
    error: Option<String>,
    config_error: Option<String>,
    last_start_attempt: Option<Instant>,
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

impl App {
    fn load_config(&mut self) -> Option<Config> {
        self.config_mtime = mtime(&self.opts.config_path);
        match Config::load_or_create(&self.opts.config_path) {
            Ok(c) => {
                self.config_error = None;
                Some(c)
            }
            Err(e) => {
                tracing::error!("config error: {e:#}");
                self.config_error = Some(format!("config error: {e:#}"));
                None
            }
        }
    }

    async fn reload(&mut self) {
        let Some(cfg) = self.load_config() else {
            return; // keep running with the previous config
        };
        if self.config.as_ref() == Some(&cfg) && (self.node.is_some() || !cfg.is_configured()) {
            return;
        }
        tracing::info!(
            "applying configuration from {}",
            self.opts.config_path.display()
        );
        self.config = Some(cfg);
        self.restart().await;
    }

    async fn restart(&mut self) {
        if let Some(n) = self.node.take() {
            n.shutdown().await;
        }
        self.error = None;
        self.last_start_attempt = Some(Instant::now());
        let Some(cfg) = self.config.clone() else {
            return;
        };
        if !cfg.is_configured() {
            tracing::warn!(
                "passphrase not set; edit {} or open Settings",
                self.opts.config_path.display()
            );
            return;
        }
        let (clipboard, changes) = match &self.opts.clipboard {
            Ok((c, ch)) => (c.clone(), ch.clone()),
            Err(e) => {
                self.error = Some(format!("clipboard unavailable: {e}"));
                return;
            }
        };
        let cipher = match &self.cipher {
            Some((pass, c)) if *pass == cfg.passphrase => c.clone(),
            _ => {
                let pass = cfg.passphrase.clone();
                let derived =
                    tokio::task::spawn_blocking(move || Cipher::from_passphrase(&pass)).await;
                match derived {
                    Ok(Ok(c)) => {
                        self.cipher = Some((cfg.passphrase.clone(), c.clone()));
                        c
                    }
                    Ok(Err(e)) => {
                        self.error = Some(format!("{e:#}"));
                        return;
                    }
                    Err(e) => {
                        self.error = Some(format!("key derivation crashed: {e}"));
                        return;
                    }
                }
            }
        };
        let mut params = NodeParams::new(cfg, cipher, self.paused.subscribe());
        // Debug knob: e.g. CLIPBRIDGE_BIND=127.0.0.1 for local experiments without firewall prompts.
        if let Some(ip) = std::env::var("CLIPBRIDGE_BIND")
            .ok()
            .and_then(|s| s.parse().ok())
        {
            params.bind_ip = ip;
        }
        match node::start(params, clipboard, changes).await {
            Ok(n) => {
                tracing::info!("sync running on {}", n.local_addr);
                self.node = Some(n);
            }
            Err(e) => {
                tracing::error!("cannot start sync: {e:#}");
                self.error = Some(format!("{e:#}"));
            }
        }
    }

    fn status(&self) -> AppStatus {
        let cfg = self.config.clone().unwrap_or_default();
        let paused = *self.paused.borrow();
        let (net, stats) = match &self.node {
            Some(n) => (n.net_status.borrow().clone(), n.stats.borrow().clone()),
            None => Default::default(),
        };
        let error = self.config_error.as_ref().or(self.error.as_ref());
        let (state, summary) = if let Some(e) = error {
            (State::Error, format!("Error: {e}"))
        } else if self.config.is_none() {
            (State::Starting, "Starting…".to_string())
        } else if !cfg.is_configured() {
            (
                State::NotConfigured,
                "Not configured: set a passphrase in Settings".to_string(),
            )
        } else if paused {
            (State::Paused, "Paused".to_string())
        } else if self.node.is_none() {
            (State::Starting, "Starting…".to_string())
        } else {
            let n = net.peers.len();
            let s = match n {
                0 => "Active: no peers connected".to_string(),
                1 => format!("Active: connected to {}", net.peers[0].name),
                _ => format!("Active: {n} peers connected"),
            };
            (State::Active, s)
        };
        AppStatus {
            state,
            summary,
            error: error.cloned(),
            device_name: cfg.device_name,
            device_id: cfg.device_id,
            listen_port: net.listen_port,
            peers: net.peers,
            pending: net.pending,
            stats,
            config_path: self.opts.config_path.display().to_string(),
            log_dir: self.opts.log_dir.display().to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    fn set_paused(&mut self, paused: bool) {
        if *self.paused.borrow() != paused {
            tracing::info!("sync {}", if paused { "paused" } else { "resumed" });
            self.paused.send_replace(paused);
        }
    }
}

/// Runs until `UiCmd::Quit`. `sink` receives status updates and UI requests.
pub async fn run(
    opts: AppOptions,
    mut ui: mpsc::Receiver<UiCmd>,
    sink: impl Fn(AppEvent) + Send + 'static,
) {
    let (paused, _) = watch::channel(false);
    let mut app = App {
        opts,
        config: None,
        config_mtime: None,
        cipher: None,
        node: None,
        paused,
        error: None,
        config_error: None,
        last_start_attempt: None,
    };
    let cancel = CancellationToken::new();
    let (ctl_tx, mut ctl_rx) = mpsc::channel::<ControlRequest>(8);
    match control::serve(&app.opts.config_path, ctl_tx, cancel.clone()).await {
        Ok(fut) => {
            tokio::spawn(fut);
        }
        Err(e) => tracing::warn!("control channel unavailable (settings window won't work): {e:#}"),
    }

    app.reload().await;
    let mut last_status: Option<AppStatus> = None;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            cmd = ui.recv() => match cmd {
                Some(UiCmd::SetPaused(p)) => app.set_paused(p),
                Some(UiCmd::Quit) | None => break,
            },
            Some(req) = ctl_rx.recv() => {
                match req.cmd {
                    Command::Status => {}
                    Command::Reload => app.reload().await,
                    Command::Pause => app.set_paused(true),
                    Command::Resume => app.set_paused(false),
                    Command::OpenSettings => sink(AppEvent::OpenSettings),
                }
                let _ = req.reply.send(Response {
                    ok: true,
                    status: Some(Box::new(app.status())),
                    error: None,
                });
            }
            _ = tick.tick() => {
                // Pick up hand edits of the config file.
                if mtime(&app.opts.config_path) != app.config_mtime {
                    app.reload().await;
                } else if app.node.is_none()
                    && app.error.is_some()
                    && app.last_start_attempt.is_none_or(|t| t.elapsed() > RETRY_START)
                {
                    app.restart().await; // e.g. port was busy
                }
            }
        }
        let status = app.status();
        if last_status.as_ref() != Some(&status) {
            tracing::debug!("status: {}", status.summary);
            sink(AppEvent::Status(Box::new(status.clone())));
            last_status = Some(status);
        }
    }

    tracing::info!("shutting down");
    cancel.cancel();
    if let Some(n) = app.node.take() {
        n.shutdown().await;
    }
}
