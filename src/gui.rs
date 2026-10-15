//! Settings / troubleshooting window. Runs as a separate process (`--settings`) so the
//! background process never carries a GPU context; talks to it over `control`.
//!
//! Three tabs: Overview (status, devices, updates), Settings (the everyday options)
//! and Advanced (network, limits, logs). Edits are saved from one bar at the bottom
//! that appears only when something changed.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use eframe::egui;

use crate::app::{AppStatus, State};
use crate::config::{self, Config, MAX_IMAGE_MB_CAP};
use crate::control::{self, Command};
use crate::logging;

const LOG_LINES: usize = 80;

const GREEN: egui::Color32 = egui::Color32::from_rgb(46, 160, 91);
const AMBER: egui::Color32 = egui::Color32::from_rgb(224, 154, 42);
const RED: egui::Color32 = egui::Color32::from_rgb(214, 69, 69);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Overview,
    Settings,
    Advanced,
}

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

fn state_color(state: State) -> egui::Color32 {
    match state {
        State::Active => GREEN,
        State::Paused => egui::Color32::GRAY,
        State::NotConfigured | State::Starting => AMBER,
        State::Error => RED,
    }
}

/// A titled group of controls.
fn section(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(6.0);
    ui.label(egui::RichText::new(title).strong().size(15.0));
    ui.add_space(2.0);
    card(ui, add);
}

/// A full-width framed box.
fn card(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    const MARGIN: i8 = 10;
    // Frame width = content + 2 × (margin + 1 px stroke); keep it inside the window.
    let width = ui.available_width() - 2.0 * (MARGIN as f32 + 1.0);
    egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::same(MARGIN))
        .show(ui, |ui| {
            ui.set_width(width);
            add(ui);
        });
}

/// Grey explanatory text under a control.
fn hint(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).small().weak());
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
    tab: Tab,
    /// What is on disk; `cfg` + `peers_text` are the edits.
    saved: Config,
    cfg: Config,
    peers_text: String,
    show_pass: bool,
    autostart: bool,
    message: Option<(bool, String, Instant)>,
    focused: bool,
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

fn parse_peers(text: &str) -> Vec<String> {
    text.split([',', '\n', ' '])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

impl SettingsApp {
    fn flash(&mut self, ok: bool, text: impl Into<String>) {
        self.message = Some((ok, text.into(), Instant::now()));
    }

    /// The config as it would be saved now.
    fn edited(&self) -> Config {
        let mut cfg = self.cfg.clone();
        cfg.manual_peers = parse_peers(&self.peers_text);
        cfg.passphrase = cfg.passphrase.trim().to_string();
        cfg
    }

    fn dirty(&self) -> bool {
        self.edited() != self.saved
    }

    fn save(&mut self) {
        let cfg = self.edited();
        if let Some(bad) = cfg.manual_peers.iter().find(|p| !p.contains(':')) {
            self.flash(false, format!("Peer \"{bad}\" must be host:port"));
            return;
        }
        if let Err(e) = cfg.save(&self.config_path) {
            self.flash(false, format!("Save failed: {e:#}"));
            return;
        }
        self.cfg = cfg.clone();
        self.saved = cfg;
        match control::request(&self.config_path, Command::Reload) {
            Ok(_) => self.flash(true, "Saved and applied"),
            Err(_) => self.flash(true, "Saved (ClipBridge is not running)"),
        }
    }

    fn revert(&mut self) {
        if let Ok(c) = Config::load_or_create(&self.config_path) {
            self.peers_text = c.manual_peers.join("\n");
            self.cfg = c.clone();
            self.saved = c;
        }
    }

    fn test_peers(&self) {
        let peers = parse_peers(&self.peers_text);
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

    fn start_background(&mut self) {
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

    fn passphrase_field(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.cfg.passphrase)
                    .password(!self.show_pass)
                    .desired_width(260.0),
            );
            ui.checkbox(&mut self.show_pass, "show");
        });
    }

    // --- Overview --------------------------------------------------------------

    fn overview(&mut self, ui: &mut egui::Ui, status: &Option<Result<AppStatus, String>>) {
        let Some(Ok(s)) = status else {
            ui.add_space(8.0);
            ui.label(if status.is_none() {
                "Connecting to ClipBridge…"
            } else {
                "ClipBridge isn't running in the background, so nothing is syncing."
            });
            return;
        };

        // Status card.
        card(ui, |ui| {
            ui.horizontal(|ui| {
                dot(ui, state_color(s.state), true);
                ui.label(
                    egui::RichText::new(&s.summary)
                        .size(17.0)
                        .color(state_color(s.state)),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let paused = s.state == State::Paused;
                    if matches!(s.state, State::Active | State::Paused)
                        && ui.button(if paused { "Resume" } else { "Pause" }).clicked()
                    {
                        let cmd = if paused {
                            Command::Resume
                        } else {
                            Command::Pause
                        };
                        let _ = control::request(&self.config_path, cmd);
                    }
                });
            });
            let last = s
                .stats
                .last_event
                .as_ref()
                .map(|e| format!("  ·  last: {e}"))
                .unwrap_or_default();
            hint(
                ui,
                &format!(
                    "{}  ·  sent {}  ·  received {}{last}",
                    s.device_name, s.stats.sent, s.stats.applied
                ),
            );
        });

        // First-run: ask for the passphrase right here.
        if s.state == State::NotConfigured {
            section(ui, "Get started", |ui| {
                ui.label("Choose a passphrase and use the same one on every computer.");
                ui.add_space(4.0);
                self.passphrase_field(ui);
                ui.add_space(4.0);
                if ui.button("Save and start syncing").clicked() {
                    self.save();
                }
            });
        }

        section(ui, "Devices", |ui| {
            if s.peers.is_empty() && s.pending.is_empty() {
                ui.label("No other devices yet.");
                hint(
                    ui,
                    "They appear here automatically when ClipBridge runs on them with the same \
                     passphrase on this network. If not, add them under Advanced → Manual peers.",
                );
                return;
            }
            egui::Grid::new("peers")
                .num_columns(4)
                .spacing([10.0, 6.0])
                .show(ui, |ui| {
                    for p in &s.peers {
                        dot(ui, GREEN, true);
                        ui.label(egui::RichText::new(&p.name).strong());
                        ui.label(egui::RichText::new(&p.addr).weak());
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
                        // Full error on hover; the row shows only its start.
                        let short = p.state.split(" (").next().unwrap_or(&p.state);
                        ui.add(egui::Label::new(egui::RichText::new(short).weak()).truncate())
                            .on_hover_text(&p.state);
                        ui.end_row();
                    }
                });
        });

        section(ui, "Version", |ui| self.update_row(ui, s));
    }

    fn update_row(&mut self, ui: &mut egui::Ui, s: &AppStatus) {
        let u = &s.update;
        ui.horizontal(|ui| {
            ui.label(format!("ClipBridge {}", s.version));
            match &u.available {
                Some(info) => {
                    ui.colored_label(GREEN, format!("·  {} is available", info.version));
                    let label = if info.can_install() {
                        "Update now"
                    } else {
                        "Download…"
                    };
                    if ui.add_enabled(!u.busy, egui::Button::new(label)).clicked() {
                        let _ = control::request(&self.config_path, Command::InstallUpdate);
                    }
                }
                None => {
                    if ui
                        .add_enabled(!u.busy, egui::Button::new("Check for updates"))
                        .clicked()
                    {
                        let _ = control::request(&self.config_path, Command::CheckUpdates);
                    }
                }
            }
        });
        if let Some(note) = &u.note {
            hint(ui, note);
        }
    }

    // --- Settings --------------------------------------------------------------

    fn settings(&mut self, ui: &mut egui::Ui) {
        section(ui, "This device", |ui| {
            egui::Grid::new("device")
                .num_columns(2)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    ui.label("Name");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.cfg.device_name).desired_width(260.0),
                    );
                    ui.end_row();
                    ui.label("Passphrase");
                    self.passphrase_field(ui);
                    ui.end_row();
                });
            hint(
                ui,
                "Use the same passphrase on every computer that should share a clipboard.",
            );
        });

        section(ui, "What to sync", |ui| {
            ui.checkbox(&mut self.cfg.sync_images, "Images");
            ui.checkbox(
                &mut self.cfg.sync_html,
                "Formatting (rich text from browsers, Word, Docs…)",
            );
            hint(ui, "Plain text always syncs.");
        });

        section(ui, "General", |ui| {
            if ui.checkbox(&mut self.autostart, "Start at login").changed() {
                let want = self.autostart;
                if let Err(e) = crate::tray::set_autostart(&self.passthrough, want) {
                    self.autostart = !want;
                    self.flash(false, format!("{e:#}"));
                }
            }
            ui.checkbox(&mut self.cfg.check_updates, "Check for updates");
            ui.add_enabled(
                self.cfg.check_updates,
                egui::Checkbox::new(&mut self.cfg.auto_update, "Install updates automatically"),
            );
        });
    }

    // --- Advanced --------------------------------------------------------------

    fn advanced(
        &mut self,
        ui: &mut egui::Ui,
        logs: &[String],
        tests: &[(String, Result<u128, String>)],
        testing: bool,
    ) {
        section(ui, "Network", |ui| {
            egui::Grid::new("net")
                .num_columns(2)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    ui.label("Port");
                    ui.add(egui::DragValue::new(&mut self.cfg.port).range(1024..=65535));
                    ui.end_row();
                    ui.label("Discovery");
                    ui.checkbox(&mut self.cfg.discovery, "Find devices automatically (mDNS)");
                    ui.end_row();
                    ui.label("Manual peers");
                    ui.add(
                        egui::TextEdit::multiline(&mut self.peers_text)
                            .desired_rows(2)
                            .desired_width(260.0)
                            .hint_text("192.168.1.20:47821"),
                    );
                    ui.end_row();
                });
            hint(
                ui,
                "Manual peers are for networks that block discovery: one host:port per line. \
                 Adding a device on one side is enough.",
            );
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(!testing, egui::Button::new("Test manual peers"))
                    .clicked()
                {
                    self.test_peers();
                }
                if testing {
                    ui.spinner();
                }
            });
            for (peer, r) in tests {
                match r {
                    Ok(ms) => ui.colored_label(GREEN, format!("{peer}: reachable ({ms} ms)")),
                    Err(e) => ui.colored_label(RED, format!("{peer}: {e}")),
                };
            }
        });

        section(ui, "Limits", |ui| {
            ui.horizontal(|ui| {
                ui.label("Largest image to sync");
                ui.add(
                    egui::DragValue::new(&mut self.cfg.max_image_mb)
                        .range(1..=MAX_IMAGE_MB_CAP)
                        .suffix(" MB"),
                );
            });
            hint(
                ui,
                "Size after PNG compression. Text and formatting are limited to 10 MB each.",
            );
        });

        section(ui, "Troubleshooting", |ui| {
            ui.horizontal(|ui| {
                ui.label("Device ID");
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(&self.saved.device_id)
                            .monospace()
                            .weak(),
                    )
                    .selectable(true),
                );
            });
            ui.horizontal(|ui| {
                if ui.button("Open logs folder").clicked() {
                    let _ = std::fs::create_dir_all(&self.log_dir);
                    crate::tray::open_path(&self.log_dir, false);
                }
                if ui.button("Open config file").clicked() {
                    crate::tray::open_path(&self.config_path, true);
                }
            });
            ui.add_space(4.0);
            egui::ScrollArea::both()
                .id_salt("log")
                .max_height(220.0)
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
    }
}

impl eframe::App for SettingsApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // A process started from the tray may open behind the active window (Windows
        // focus-stealing rules); ask for focus once.
        if !self.focused {
            self.focused = true;
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        let (status, logs, tests, testing) = {
            let s = lock(&self.shared);
            (
                s.status.clone(),
                s.log_tail.clone(),
                s.peer_tests.clone(),
                s.testing,
            )
        };

        egui::Panel::top("tabs").show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                for (tab, label) in [
                    (Tab::Overview, "Overview"),
                    (Tab::Settings, "Settings"),
                    (Tab::Advanced, "Advanced"),
                ] {
                    let text = egui::RichText::new(label).size(15.0);
                    ui.selectable_value(&mut self.tab, tab, text);
                }
            });
            ui.add_space(4.0);
            // The background process is down: say so on every tab.
            if let Some(Err(_)) = &status {
                ui.horizontal(|ui| {
                    dot(ui, RED, true);
                    ui.colored_label(RED, "ClipBridge is not running");
                    if ui.small_button("Start it").clicked() {
                        self.start_background();
                    }
                });
                ui.add_space(4.0);
            }
        });

        let flash = self
            .message
            .as_ref()
            .filter(|(_, _, at)| at.elapsed() < Duration::from_secs(5))
            .cloned();
        if self.dirty() || flash.is_some() {
            egui::Panel::bottom("save").show(ui, |ui| {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if self.dirty() {
                        ui.label("Unsaved changes");
                        if ui.button("Save").clicked() {
                            self.save();
                        }
                        if ui.button("Revert").clicked() {
                            self.revert();
                        }
                    }
                    if let Some((ok, text, _)) = &flash {
                        ui.colored_label(if *ok { GREEN } else { RED }, text);
                    }
                });
                ui.add_space(4.0);
            });
            if flash.is_some() {
                ui.ctx().request_repaint_after(Duration::from_millis(500));
            }
        }

        egui::CentralPanel::default_margins()
            .frame(egui::Frame::central_panel(ui.style()))
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink(false)
                    .show(ui, |ui| match self.tab {
                        Tab::Overview => self.overview(ui, &status),
                        Tab::Settings => self.settings(ui),
                        Tab::Advanced => self.advanced(ui, &logs, &tests, testing),
                    });
            });
    }
}

pub fn run(config_path: PathBuf, passthrough: Vec<String>) -> anyhow::Result<()> {
    let cfg = Config::load_or_create(&config_path)?;
    let log_dir = config::default_log_dir();
    let autostart = crate::tray::autostart_enabled(&passthrough);
    #[allow(unused_mut)]
    let mut options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ClipBridge")
            .with_inner_size([520.0, 600.0])
            .with_min_inner_size([440.0, 420.0])
            // Our icon, not eframe's default. On macOS it becomes the Dock icon, so use
            // the same rounded-square artwork as the .app.
            .with_icon(std::sync::Arc::new(egui::IconData {
                rgba: if cfg!(target_os = "macos") {
                    crate::icon::render_macos_app_icon(512)
                } else {
                    crate::icon::render_tile_icon(256)
                },
                width: if cfg!(target_os = "macos") { 512 } else { 256 },
                height: if cfg!(target_os = "macos") { 512 } else { 256 },
            })),
        ..Default::default()
    };
    // The .app is menu-bar only (LSUIElement), and this process inherits that. Make the
    // settings window a regular app so it shows in the Dock and Cmd+Tab while open.
    #[cfg(target_os = "macos")]
    {
        options.event_loop_builder = Some(Box::new(|builder| {
            use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
            builder.with_activation_policy(ActivationPolicy::Regular);
        }));
    }
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
            // Overview also hosts the first-run passphrase prompt.
            let tab = Tab::Overview;
            Ok(Box::new(SettingsApp {
                peers_text: cfg.manual_peers.join("\n"),
                saved: cfg.clone(),
                cfg,
                config_path,
                log_dir,
                passthrough,
                tab,
                show_pass: false,
                autostart,
                message: None,
                focused: false,
                shared,
            }))
        }),
    )
    .map_err(|e| anyhow::anyhow!("settings window: {e}"))
}
