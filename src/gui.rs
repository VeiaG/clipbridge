//! Settings / troubleshooting window. Runs as a separate process (`--settings`) so the
//! background process never carries a GPU context; talks to it over `control`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use eframe::egui;

use crate::app::{AppStatus, State};
use crate::config::{self, Config, MAX_IMAGE_MB_CAP, PLACEHOLDER_PASSPHRASE};
use crate::control::{self, Command};
use crate::logging;

const LOG_LINES: usize = 60;

const GREEN: egui::Color32 = egui::Color32::from_rgb(46, 160, 91);
const AMBER: egui::Color32 = egui::Color32::from_rgb(224, 154, 42);
const RED: egui::Color32 = egui::Color32::from_rgb(214, 69, 69);

/// Status dot, painted (the default egui font has no "●" glyph).
fn dot(ui: &mut egui::Ui, color: egui::Color32, filled: bool) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
    let (c, r) = (rect.center(), 4.5);
    if filled {
        ui.painter().circle_filled(c, r, color);
    } else {
        ui.painter()
            .circle_stroke(c, r, egui::Stroke::new(1.5, color));
    }
}

#[derive(Default)]
struct Shared {
    status: Option<Result<AppStatus, String>>,
    log_tail: Vec<String>,
    peer_tests: Vec<(String, Result<u128, String>)>,
    testing: bool,
}

struct SettingsApp {
    config_path: PathBuf,
    log_dir: PathBuf,
    passthrough: Vec<String>,
    cfg: Config,
    peers_text: String,
    show_pass: bool,
    message: Option<(bool, String, Instant)>,
    shared: Arc<Mutex<Shared>>,
}

fn lock(s: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    s.lock().unwrap_or_else(|e| e.into_inner())
}

fn tail_lines(path: &std::path::Path, n: usize) -> Vec<String> {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(64 * 1024);
    let mut buf = Vec::new();
    if f.seek(SeekFrom::Start(start)).is_err() || f.read_to_end(&mut buf).is_err() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&buf);
    let lines: Vec<String> = text.lines().map(str::to_string).collect();
    lines[lines.len().saturating_sub(n)..].to_vec()
}

/// Background poller: status every second, log tail every two.
fn spawn_poller(
    ctx: egui::Context,
    config_path: PathBuf,
    log_dir: PathBuf,
    shared: Arc<Mutex<Shared>>,
) {
    std::thread::spawn(move || {
        let mut n = 0u64;
        loop {
            let status = control::request(&config_path, Command::Status)
                .and_then(|r| {
                    r.status
                        .map(|s| *s)
                        .ok_or_else(|| anyhow::anyhow!("no status"))
                })
                .map_err(|e| format!("{e:#}"));
            let logs = n.is_multiple_of(2).then(|| {
                logging::latest_log_file(&log_dir)
                    .map(|p| tail_lines(&p, LOG_LINES))
                    .unwrap_or_default()
            });
            {
                let mut s = lock(&shared);
                s.status = Some(status);
                if let Some(l) = logs {
                    s.log_tail = l;
                }
            }
            ctx.request_repaint();
            n += 1;
            std::thread::sleep(Duration::from_secs(1));
        }
    });
}

fn ago(unix: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(unix);
    let s = now.saturating_sub(unix);
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        _ => format!("{}h {}m", s / 3600, s / 60 % 60),
    }
}

impl SettingsApp {
    fn flash(&mut self, ok: bool, text: impl Into<String>) {
        self.message = Some((ok, text.into(), Instant::now()));
    }

    fn save(&mut self) {
        let mut cfg = self.cfg.clone();
        cfg.manual_peers = self
            .peers_text
            .split([',', '\n', ' '])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        cfg.passphrase = cfg.passphrase.trim().to_string();
        if let Some(bad) = cfg.manual_peers.iter().find(|p| !p.contains(':')) {
            self.flash(false, format!("Peer \"{bad}\" must be host:port"));
            return;
        }
        if let Err(e) = cfg.save(&self.config_path) {
            self.flash(false, format!("Save failed: {e:#}"));
            return;
        }
        self.cfg = cfg;
        match control::request(&self.config_path, Command::Reload) {
            Ok(_) => self.flash(true, "Saved and applied"),
            Err(_) => self.flash(true, "Saved (ClipBridge is not running)"),
        }
    }

    fn test_peers(&self) {
        let peers: Vec<String> = self
            .peers_text
            .split([',', '\n', ' '])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        let shared = self.shared.clone();
        lock(&shared).testing = true;
        std::thread::spawn(move || {
            use std::net::ToSocketAddrs;
            let results = peers
                .into_iter()
                .map(|p| {
                    let r = p
                        .to_socket_addrs()
                        .map_err(|e| format!("resolve: {e}"))
                        .and_then(|mut a| a.next().ok_or_else(|| "no address".to_string()))
                        .and_then(|addr| {
                            let t = Instant::now();
                            std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(2))
                                .map(|_| t.elapsed().as_millis())
                                .map_err(|e| e.to_string())
                        });
                    (p, r)
                })
                .collect();
            let mut s = lock(&shared);
            s.peer_tests = results;
            s.testing = false;
        });
    }

    fn status_section(&mut self, ui: &mut egui::Ui, status: &Option<Result<AppStatus, String>>) {
        match status {
            None => {
                ui.label("Connecting to ClipBridge…");
            }
            Some(Err(_)) => {
                ui.horizontal(|ui| {
                    dot(ui, RED, true);
                    ui.colored_label(RED, "ClipBridge is not running");
                    if ui.button("Start it").clicked() {
                        match std::env::current_exe() {
                            Ok(exe) => {
                                if let Err(e) = std::process::Command::new(exe)
                                    .args(&self.passthrough)
                                    .spawn()
                                {
                                    self.flash(false, format!("Start failed: {e}"));
                                }
                            }
                            Err(e) => self.flash(false, format!("Start failed: {e}")),
                        }
                    }
                });
            }
            Some(Ok(s)) => {
                let color = match s.state {
                    State::Active => GREEN,
                    State::Paused => egui::Color32::GRAY,
                    State::NotConfigured | State::Starting => AMBER,
                    State::Error => RED,
                };
                ui.horizontal(|ui| {
                    dot(ui, color, true);
                    ui.colored_label(color, &s.summary);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let paused = s.state == State::Paused;
                        if ui.button(if paused { "Resume" } else { "Pause" }).clicked() {
                            let cmd = if paused {
                                Command::Resume
                            } else {
                                Command::Pause
                            };
                            let _ = control::request(&self.config_path, cmd);
                        }
                    });
                });
                ui.label(format!(
                    "This device: {}  ·  port {}  ·  sent {}  ·  received {}{}",
                    s.device_name,
                    s.listen_port,
                    s.stats.sent,
                    s.stats.applied,
                    s.stats
                        .last_event
                        .as_ref()
                        .map(|e| format!("  ·  last: {e}"))
                        .unwrap_or_default()
                ));
            }
        }
    }

    fn peers_section(&self, ui: &mut egui::Ui, status: &Option<Result<AppStatus, String>>) {
        let Some(Ok(s)) = status else { return };
        if s.peers.is_empty() && s.pending.is_empty() {
            ui.label("No peers yet. Other machines appear here automatically when they run ClipBridge with the same passphrase on this network.");
            return;
        }
        egui::Grid::new("peers")
            .num_columns(4)
            .striped(true)
            .show(ui, |ui| {
                for p in &s.peers {
                    dot(ui, GREEN, true);
                    ui.label(&p.name);
                    ui.label(&p.addr);
                    ui.label(format!(
                        "{}  ·  up {}",
                        p.rtt_ms
                            .map(|r| format!("{r} ms"))
                            .unwrap_or_else(|| "—".into()),
                        ago(p.connected_at)
                    ));
                    ui.end_row();
                }
                for p in &s.pending {
                    dot(ui, egui::Color32::GRAY, false);
                    ui.label(&p.label);
                    ui.label("");
                    ui.label(&p.state);
                    ui.end_row();
                }
            });
    }

    fn settings_section(&mut self, ui: &mut egui::Ui) {
        egui::Grid::new("settings")
            .num_columns(2)
            .spacing([12.0, 8.0])
            .show(ui, |ui| {
                ui.label("Device name");
                ui.text_edit_singleline(&mut self.cfg.device_name);
                ui.end_row();

                ui.label("Passphrase");
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.cfg.passphrase)
                            .password(!self.show_pass),
                    );
                    ui.checkbox(&mut self.show_pass, "show");
                });
                ui.end_row();

                ui.label("Port");
                ui.add(egui::DragValue::new(&mut self.cfg.port).range(1024..=65535));
                ui.end_row();

                ui.label("Images");
                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.cfg.sync_images, "sync images, up to");
                    ui.add(
                        egui::DragValue::new(&mut self.cfg.max_image_mb)
                            .range(1..=MAX_IMAGE_MB_CAP)
                            .suffix(" MB"),
                    );
                });
                ui.end_row();

                ui.label("Discovery");
                ui.checkbox(&mut self.cfg.discovery, "find peers automatically (mDNS)");
                ui.end_row();

                ui.label("Manual peers");
                ui.add(
                    egui::TextEdit::multiline(&mut self.peers_text)
                        .desired_rows(2)
                        .hint_text("192.168.1.20:47821, one per line"),
                );
                ui.end_row();
            });
        if self.cfg.passphrase.trim().is_empty() || self.cfg.passphrase == PLACEHOLDER_PASSPHRASE {
            ui.colored_label(
                AMBER,
                "Set a passphrase (the same on every machine) to start syncing.",
            );
        }
    }
}

impl eframe::App for SettingsApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let (status, logs, tests, testing) = {
            let s = lock(&self.shared);
            (
                s.status.clone(),
                s.log_tail.clone(),
                s.peer_tests.clone(),
                s.testing,
            )
        };
        egui::Frame::central_panel(ui.style()).show(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink(false)
                .show(ui, |ui| {
                    ui.heading("ClipBridge");
                    self.status_section(ui, &status);
                    ui.separator();

                    ui.strong("Settings");
                    self.settings_section(ui);
                    ui.horizontal(|ui| {
                        if ui.button("Save").clicked() {
                            self.save();
                        }
                        if ui.button("Revert").clicked()
                            && let Ok(c) = Config::load_or_create(&self.config_path)
                        {
                            self.peers_text = c.manual_peers.join("\n");
                            self.cfg = c;
                        }
                        if ui
                            .add_enabled(!testing, egui::Button::new("Test manual peers"))
                            .clicked()
                        {
                            self.test_peers();
                        }
                        if let Some((ok, text, at)) = &self.message
                            && at.elapsed() < Duration::from_secs(6)
                        {
                            let c = if *ok { GREEN } else { RED };
                            ui.colored_label(c, text);
                        }
                    });
                    for (peer, r) in &tests {
                        match r {
                            Ok(ms) => ui.label(format!("OK  {peer}: reachable ({ms} ms)")),
                            Err(e) => ui.colored_label(RED, format!("FAIL  {peer}: {e}")),
                        };
                    }
                    ui.separator();

                    ui.strong("Peers");
                    self.peers_section(ui, &status);
                    ui.separator();

                    ui.horizontal(|ui| {
                        ui.strong("Recent log");
                        if ui.small_button("Open logs folder").clicked() {
                            let _ = std::fs::create_dir_all(&self.log_dir);
                            crate::tray::open_path(&self.log_dir, false);
                        }
                        if ui.small_button("Open config file").clicked() {
                            crate::tray::open_path(&self.config_path, true);
                        }
                    });
                    egui::ScrollArea::both()
                        .id_salt("log")
                        .max_height(180.0)
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            let text = if logs.is_empty() {
                                "(no log yet)".to_string()
                            } else {
                                logs.join("\n")
                            };
                            ui.add(
                                egui::Label::new(egui::RichText::new(text).monospace().size(11.0))
                                    .wrap_mode(egui::TextWrapMode::Extend),
                            );
                        });
                });
        });
    }
}

pub fn run(config_path: PathBuf, passthrough: Vec<String>) -> anyhow::Result<()> {
    let cfg = Config::load_or_create(&config_path)?;
    let log_dir = config::default_log_dir();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ClipBridge Settings")
            .with_inner_size([560.0, 720.0])
            .with_min_inner_size([420.0, 400.0]),
        ..Default::default()
    };
    eframe::run_native(
        "ClipBridge Settings",
        options,
        Box::new(move |cc| {
            let shared = Arc::new(Mutex::new(Shared::default()));
            spawn_poller(
                cc.egui_ctx.clone(),
                config_path.clone(),
                log_dir.clone(),
                shared.clone(),
            );
            Ok(Box::new(SettingsApp {
                peers_text: cfg.manual_peers.join("\n"),
                cfg,
                config_path,
                log_dir,
                passthrough,
                show_pass: false,
                message: None,
                shared,
            }))
        }),
    )
    .map_err(|e| anyhow::anyhow!("settings window: {e}"))
}
