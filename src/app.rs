//! App supervisor: owns the config, (re)starts the sync node, serves the control
//! channel, checks for updates and publishes status to the tray.

use std::path::{Path, PathBuf};
use std::sync::Arc;
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
use crate::transfer::{Hub, HubEvent, ReceiveSettings, TransferInfo, TransferState, Watcher};
use crate::update::{self, UpdateInfo};

const RETRY_START: Duration = Duration::from_secs(10);
const FIRST_UPDATE_CHECK: Duration = Duration::from_secs(20);
const UPDATE_CHECK_EVERY: Duration = Duration::from_secs(12 * 3600);
/// After a failed check, try again sooner than the regular interval.
const UPDATE_RETRY: Duration = Duration::from_secs(3600);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Starting,
    NotConfigured,
    Active,
    Paused,
    Error,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateStatus {
    /// A newer version that is available.
    pub available: Option<UpdateInfo>,
    /// A check or install is running.
    pub busy: bool,
    /// Last outcome worth showing ("Up to date", "Update failed: …").
    pub note: Option<String>,
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
    #[serde(default)]
    pub update: UpdateStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiCmd {
    SetPaused(bool),
    /// Install the available update, or check for one if none is known.
    Update,
    Quit,
}

#[derive(Debug, Clone)]
pub enum AppEvent {
    Status(Box<AppStatus>),
    /// Someone (a second launch) asked to show the settings window.
    OpenSettings,
    /// A file offer waits for the user: show the incoming-files window.
    OpenIncoming,
}

pub struct AppOptions {
    pub config_path: PathBuf,
    pub log_dir: PathBuf,
    /// `Err` if the system clipboard could not be opened.
    pub clipboard: Result<(ClipboardHandle, watch::Receiver<u64>), String>,
}

/// Results of update work done on blocking threads.
enum UpdateEvent {
    Checked {
        result: Result<Option<UpdateInfo>, String>,
        manual: bool,
    },
    Installed(Result<(), String>),
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
    started: Instant,
    update: UpdateStatus,
    next_update_check: Option<Instant>,
    update_tx: mpsc::Sender<UpdateEvent>,
    /// Set once an update has been handed to the installer; the app then exits.
    quit_for_update: bool,
    hub: Arc<Hub>,
    /// An automatic update is waiting for transfers to finish.
    install_when_idle: bool,
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Whether two configs differ only in settings that don't affect syncing.
fn same_sync_settings(a: &Config, b: &Config) -> bool {
    let strip = |c: &Config| Config {
        check_updates: false,
        auto_update: false,
        // Read live by the transfer hub.
        receive_files: true,
        ask_before_receive: false,
        receive_dir: String::new(),
        ..c.clone()
    };
    strip(a) == strip(b)
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
        self.hub.set_settings(ReceiveSettings::from_config(&cfg));
        let unchanged_sync = self
            .config
            .as_ref()
            .is_some_and(|old| same_sync_settings(old, &cfg));
        if unchanged_sync && (self.node.is_some() || !cfg.is_configured()) {
            self.config = Some(cfg); // update-related settings only
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
        params.hub = self.hub.clone();
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
            version: update::current_version().to_string(),
            update: self.update.clone(),
        }
    }

    fn set_paused(&mut self, paused: bool) {
        if *self.paused.borrow() != paused {
            tracing::info!("sync {}", if paused { "paused" } else { "resumed" });
            self.paused.send_replace(paused);
        }
    }

    // --- updates -------------------------------------------------------------

    fn start_update_check(&mut self, manual: bool) {
        if self.update.busy {
            return;
        }
        self.update.busy = true;
        if manual {
            self.update.note = Some("Checking for updates…".into());
        }
        let tx = self.update_tx.clone();
        tokio::task::spawn_blocking(move || {
            let result = update::check().map_err(|e| format!("{e:#}"));
            let _ = tx.blocking_send(UpdateEvent::Checked { result, manual });
        });
    }

    fn start_update_install(&mut self) {
        if self.update.busy {
            return;
        }
        let Some(info) = self.update.available.clone() else {
            self.start_update_check(true);
            return;
        };
        if !info.can_install() {
            // Portable copy or bare binary: send the user to the release page.
            crate::tray::open_url(&info.release_url);
            return;
        }
        self.update.busy = true;
        self.update.note = Some(format!("Downloading {}…", info.version));
        tracing::info!("installing update {}", info.version);
        let tx = self.update_tx.clone();
        tokio::task::spawn_blocking(move || {
            let result = update::install(&info).map_err(|e| format!("{e:#}"));
            let _ = tx.blocking_send(UpdateEvent::Installed(result));
        });
    }

    fn on_update_event(&mut self, ev: UpdateEvent) {
        self.update.busy = false;
        match ev {
            UpdateEvent::Checked { result, manual } => {
                let now = Instant::now();
                match result {
                    Ok(Some(info)) => {
                        self.next_update_check = Some(now + UPDATE_CHECK_EVERY);
                        if self.update.available.as_ref() != Some(&info) {
                            tracing::info!("update available: {}", info.version);
                        }
                        self.update.note = None;
                        self.update.available = Some(info.clone());
                        let auto = self.config.as_ref().is_some_and(|c| c.auto_update);
                        if auto && info.can_install() {
                            if self.hub.active() {
                                // Installing restarts us, which would cut transfers off.
                                self.install_when_idle = true;
                            } else {
                                self.start_update_install();
                            }
                        }
                    }
                    Ok(None) => {
                        self.next_update_check = Some(now + UPDATE_CHECK_EVERY);
                        self.update.available = None;
                        self.update.note = manual.then(|| "Up to date".to_string());
                    }
                    Err(e) => {
                        self.next_update_check = Some(now + UPDATE_RETRY);
                        tracing::warn!("update check failed: {e}");
                        self.update.note = manual.then(|| format!("Update check failed: {e}"));
                    }
                }
            }
            UpdateEvent::Installed(Ok(())) => {
                let v = self.update.available.as_ref().map(|i| i.version.clone());
                tracing::info!("update {} handed over; restarting", v.unwrap_or_default());
                self.update.note = Some("Restarting with the new version…".into());
                self.quit_for_update = true;
            }
            UpdateEvent::Installed(Err(e)) => {
                tracing::warn!("update failed: {e}");
                self.update.note = Some(format!("Update failed: {e}"));
            }
        }
    }

    fn maybe_check_updates(&mut self) {
        if self.install_when_idle && !self.hub.active() {
            self.install_when_idle = false;
            self.start_update_install();
        }
        let enabled = self.config.as_ref().is_some_and(|c| c.check_updates);
        if !enabled || self.update.busy {
            return;
        }
        let due = match self.next_update_check {
            Some(t) => Instant::now() >= t,
            None => self.started.elapsed() >= FIRST_UPDATE_CHECK,
        };
        if due {
            self.start_update_check(false);
        }
    }
}

/// A system notification for a finished transfer, where one is useful.
fn notify_finished(hub: &Hub, t: &TransferInfo) {
    let (title, body) = if t.outgoing {
        // The Send window already shows the outcome.
        if hub.is_watched(Watcher::Send) {
            return;
        }
        match &t.state {
            TransferState::Done => ("Files delivered", format!("{} to {}", t.label, t.peer)),
            TransferState::Declined(r) => ("Files not delivered", format!("{}: {r}", t.label)),
            TransferState::Failed(r) => {
                ("Sending failed", format!("{} to {}: {r}", t.label, t.peer))
            }
            _ => return,
        }
    } else {
        match &t.state {
            TransferState::Done => ("Files received", format!("{} from {}", t.label, t.peer)),
            TransferState::Failed(r) => (
                "Receiving failed",
                format!("{} from {}: {r}", t.label, t.peer),
            ),
            _ => return,
        }
    };
    crate::notify::spawn(title, &body);
}

/// Runs until `UiCmd::Quit` (or an update takes over). `sink` receives status
/// updates and UI requests.
pub async fn run(
    opts: AppOptions,
    mut ui: mpsc::Receiver<UiCmd>,
    sink: impl Fn(AppEvent) + Send + 'static,
) {
    let (paused, _) = watch::channel(false);
    let (update_tx, mut update_rx) = mpsc::channel(4);
    let (hub, mut hub_rx) = Hub::new(ReceiveSettings::from_config(&Config::default()));
    let mut app = App {
        hub,
        install_when_idle: false,
        opts,
        config: None,
        config_mtime: None,
        cipher: None,
        node: None,
        paused,
        error: None,
        config_error: None,
        last_start_attempt: None,
        started: Instant::now(),
        update: UpdateStatus::default(),
        next_update_check: None,
        update_tx,
        quit_for_update: false,
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
                Some(UiCmd::Update) => app.start_update_install(),
                Some(UiCmd::Quit) | None => break,
            },
            Some(req) = ctl_rx.recv() => {
                let mut reply = Response {
                    ok: true,
                    status: None,
                    error: None,
                    transfers: None,
                };
                match req.cmd {
                    Command::Status => {}
                    Command::Reload => app.reload().await,
                    Command::Pause => app.set_paused(true),
                    Command::Resume => app.set_paused(false),
                    Command::OpenSettings => sink(AppEvent::OpenSettings),
                    Command::CheckUpdates => app.start_update_check(true),
                    Command::InstallUpdate => app.start_update_install(),
                    Command::SendFiles { paths, peers } => {
                        let paths = paths.into_iter().map(PathBuf::from).collect();
                        let sent = match &app.node {
                            Some(n) => n.send_files(paths, &peers).map(|_| ()),
                            None => Err("sync isn't running".to_string()),
                        };
                        if let Err(e) = sent {
                            reply.ok = false;
                            reply.error = Some(e);
                        }
                    }
                    Command::Transfers { watcher } => {
                        app.hub.watched(watcher);
                        reply.transfers = Some(app.hub.list());
                    }
                    Command::CancelTransfer { id } => app.hub.cancel(id),
                    Command::Decide { id, accept } => app.hub.decide(id, accept),
                    Command::ClearFinished { outgoing } => app.hub.clear_finished(outgoing),
                }
                reply.status = Some(Box::new(app.status()));
                let _ = req.reply.send(reply);
            }
            Some(ev) = hub_rx.recv() => match ev {
                HubEvent::Offer => sink(AppEvent::OpenIncoming),
                HubEvent::Finished(t) => notify_finished(&app.hub, &t),
            },
            Some(ev) = update_rx.recv() => app.on_update_event(ev),
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
                app.maybe_check_updates();
            }
        }
        let status = app.status();
        if last_status.as_ref() != Some(&status) {
            tracing::debug!("status: {}", status.summary);
            sink(AppEvent::Status(Box::new(status.clone())));
            last_status = Some(status);
        }
        if app.quit_for_update {
            break;
        }
    }

    tracing::info!("shutting down");
    cancel.cancel();
    if let Some(n) = app.node.take() {
        n.shutdown().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_settings_do_not_restart_sync() {
        let a = Config::default();
        let b = Config {
            auto_update: true,
            check_updates: false,
            ..a.clone()
        };
        assert!(same_sync_settings(&a, &b));
        let c = Config {
            port: a.port + 1,
            ..a.clone()
        };
        assert!(!same_sync_settings(&a, &c));
    }
}
