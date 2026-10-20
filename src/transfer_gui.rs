//! The Send window (`--send`) and the incoming-files window (`--incoming`). Like
//! Settings, each is its own short-lived process that polls the background process over
//! `control`; the transfers themselves belong to the background process, so closing a
//! window never stops one.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use eframe::egui;

use crate::app::AppStatus;
use crate::control::{self, Command};
use crate::gui::{GREEN, RED, card, dot, hint, section, window_options};
use crate::net::PeerStatus;
use crate::transfer::{TransferInfo, TransferState, Watcher};

const POLL: Duration = Duration::from_millis(500);

#[derive(Default)]
struct Shared {
    status: Option<Result<AppStatus, String>>,
    transfers: Vec<TransferInfo>,
}

fn lock(s: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    s.lock().unwrap_or_else(|e| e.into_inner())
}

fn spawn_poller(
    ctx: egui::Context,
    config_path: PathBuf,
    watcher: Watcher,
    shared: Arc<Mutex<Shared>>,
) {
    std::thread::spawn(move || {
        loop {
            let r = control::request(&config_path, Command::Transfers { watcher });
            {
                let mut s = lock(&shared);
                match r {
                    Ok(resp) => {
                        s.status = resp.status.map(|b| Ok(*b));
                        s.transfers = resp.transfers.unwrap_or_default();
                    }
                    Err(e) => {
                        s.status = Some(Err(format!("{e:#}")));
                        s.transfers.clear();
                    }
                }
            }
            ctx.request_repaint();
            std::thread::sleep(POLL);
        }
    });
}

pub(crate) fn fmt_bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f64 / 1024.0;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if v < 10.0 {
        format!("{v:.1} {}", UNITS[unit])
    } else {
        format!("{v:.0} {}", UNITS[unit])
    }
}

fn summary(t: &TransferInfo) -> String {
    match t.files {
        0 if t.total_bytes == 0 => t.label.clone(),
        1 => format!("{} ({})", t.label, fmt_bytes(t.total_bytes)),
        n => format!("{} ({n} files, {})", t.label, fmt_bytes(t.total_bytes)),
    }
}

fn progress(ui: &mut egui::Ui, t: &TransferInfo) {
    let frac = if t.total_bytes == 0 {
        0.0
    } else {
        t.done_bytes as f32 / t.total_bytes as f32
    };
    let text = format!(
        "{} of {}",
        fmt_bytes(t.done_bytes),
        fmt_bytes(t.total_bytes)
    );
    ui.add(egui::ProgressBar::new(frac).text(text));
}

/// Opens the folder that holds a received item.
fn show_in_folder(saved_to: &str) {
    let p = Path::new(saved_to);
    let dir = if p.is_dir() {
        p
    } else {
        p.parent().unwrap_or(p)
    };
    crate::tray::open_path(dir, false);
}

fn request(config_path: &Path, cmd: Command) -> Result<(), String> {
    control::request(config_path, cmd)
        .map(|_| ())
        .map_err(|e| format!("{e:#}"))
}

fn not_running(ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        dot(ui, RED, true);
        ui.colored_label(RED, "ClipBridge is not running");
    });
}

// ---------------------------------------------------------------------------------
// Send window

struct SendApp {
    config_path: PathBuf,
    /// Dropped or picked, waiting for the Send button (several devices only).
    staged: Vec<PathBuf>,
    selected: HashSet<String>,
    error: Option<String>,
    focused: bool,
    shared: Arc<Mutex<Shared>>,
}

impl SendApp {
    fn send(&mut self, peers: Vec<String>) {
        let paths = self
            .staged
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        match request(&self.config_path, Command::SendFiles { paths, peers }) {
            Ok(()) => {
                self.staged.clear();
                self.error = None;
            }
            Err(e) => self.error = Some(e),
        }
    }

    fn targets(&self, ui: &mut egui::Ui, peers: &[PeerStatus], selected: &mut HashSet<String>) {
        if let [only] = peers {
            ui.horizontal(|ui| {
                ui.label("To");
                dot(ui, GREEN, true);
                ui.label(egui::RichText::new(&only.name).strong());
            });
            return;
        }
        ui.horizontal_wrapped(|ui| {
            ui.label("To");
            let mut all = peers.iter().all(|p| selected.contains(&p.device_id));
            if ui.checkbox(&mut all, "Everyone").changed() {
                selected.clear();
                if all {
                    selected.extend(peers.iter().map(|p| p.device_id.clone()));
                }
            }
            for p in peers {
                let mut on = selected.contains(&p.device_id);
                if ui.checkbox(&mut on, &p.name).changed() {
                    if on {
                        selected.insert(p.device_id.clone());
                    } else {
                        selected.remove(&p.device_id);
                    }
                }
            }
        });
    }

    fn drop_zone(&mut self, ui: &mut egui::Ui, hovering: bool) {
        let width = ui.available_width();
        let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 96.0), egui::Sense::hover());
        let visuals = ui.visuals();
        let (fill, stroke) = if hovering {
            (
                visuals.selection.bg_fill.gamma_multiply(0.3),
                visuals.selection.stroke,
            )
        } else {
            (
                visuals.faint_bg_color,
                egui::Stroke::new(1.5, visuals.widgets.noninteractive.bg_stroke.color),
            )
        };
        let painter = ui.painter();
        painter.rect(rect, 8.0, fill, stroke, egui::StrokeKind::Inside);
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "Drop files or folders here",
            egui::FontId::proportional(16.0),
            visuals.text_color(),
        );
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui.button("Add files…").clicked()
                && let Some(files) = rfd::FileDialog::new().pick_files()
            {
                self.staged.extend(files);
            }
            if ui.button("Add folder…").clicked()
                && let Some(dirs) = rfd::FileDialog::new().pick_folders()
            {
                self.staged.extend(dirs);
            }
        });
    }

    fn transfers(&self, ui: &mut egui::Ui, transfers: &[TransferInfo]) {
        let outgoing: Vec<&TransferInfo> = transfers.iter().filter(|t| t.outgoing).rev().collect();
        if outgoing.is_empty() {
            return;
        }
        section(ui, "Sent", |ui| {
            for (i, t) in outgoing.iter().enumerate() {
                if i > 0 {
                    ui.separator();
                }
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(summary(t)).strong());
                    ui.label(format!("to {}", t.peer));
                });
                match &t.state {
                    TransferState::Connecting => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Connecting…");
                        });
                    }
                    TransferState::Waiting => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(format!("Waiting for {} to accept…", t.peer));
                        });
                    }
                    TransferState::Asking | TransferState::Running => progress(ui, t),
                    TransferState::Done => {
                        ui.colored_label(GREEN, "Delivered");
                    }
                    TransferState::Declined(r) => {
                        ui.label(format!("Not delivered: {r}"));
                    }
                    TransferState::Failed(r) => {
                        ui.colored_label(RED, format!("Failed: {r}"));
                    }
                    TransferState::Cancelled => {
                        ui.label("Cancelled");
                    }
                }
                if t.skipped > 0 {
                    hint(ui, &format!("{} symbolic link(s) left out", t.skipped));
                }
                if !t.state.finished() && ui.small_button("Cancel").clicked() {
                    let _ = request(&self.config_path, Command::CancelTransfer { id: t.id });
                }
            }
            if outgoing.iter().any(|t| t.state.finished()) {
                ui.add_space(4.0);
                if ui.button("Clear finished").clicked() {
                    let _ = request(&self.config_path, Command::ClearFinished { outgoing: true });
                }
            }
        });
    }
}

impl eframe::App for SendApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if !self.focused {
            self.focused = true;
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        let (status, transfers) = {
            let s = lock(&self.shared);
            (s.status.clone(), s.transfers.clone())
        };
        let (dropped, hovering) = ui.ctx().input(|i| {
            let dropped: Vec<PathBuf> = i
                .raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .collect();
            (dropped, !i.raw.hovered_files.is_empty())
        });
        self.staged.extend(dropped);

        let peers: Vec<PeerStatus> = match &status {
            Some(Ok(s)) => s.peers.clone(),
            _ => Vec::new(),
        };
        let mut selected = std::mem::take(&mut self.selected);
        selected.retain(|id| peers.iter().any(|p| &p.device_id == id));
        let targets: Vec<String> = match peers.as_slice() {
            [only] => vec![only.device_id.clone()],
            _ => selected.iter().cloned().collect(),
        };
        // With a single device there is nothing to choose: send right away.
        if peers.len() == 1 && !self.staged.is_empty() {
            self.send(targets.clone());
        }

        egui::CentralPanel::default_margins()
            .frame(egui::Frame::central_panel(ui.style()))
            .show(ui, |ui| {
                egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                    card(ui, |ui| {
                        match &status {
                            None => {
                                ui.label("Connecting to ClipBridge…");
                            }
                            Some(Err(_)) => not_running(ui),
                            Some(Ok(_)) if peers.is_empty() => {
                                ui.label("No devices connected.");
                                hint(
                                    ui,
                                    "Devices show up here when ClipBridge runs on them with the \
                                     same passphrase.",
                                );
                            }
                            Some(Ok(_)) => self.targets(ui, &peers, &mut selected),
                        }
                        ui.add_space(6.0);
                        self.drop_zone(ui, hovering);

                        if !self.staged.is_empty() {
                            ui.add_space(6.0);
                            let mut remove = None;
                            for (i, p) in self.staged.iter().enumerate() {
                                ui.horizontal(|ui| {
                                    if ui.small_button("×").clicked() {
                                        remove = Some(i);
                                    }
                                    let name = p
                                        .file_name()
                                        .map(|n| n.to_string_lossy().into_owned())
                                        .unwrap_or_else(|| p.display().to_string());
                                    let suffix = if p.is_dir() { "/" } else { "" };
                                    ui.label(format!("{name}{suffix}")).on_hover_text(p.display().to_string());
                                });
                            }
                            if let Some(i) = remove {
                                self.staged.remove(i);
                            }
                            ui.add_space(4.0);
                            let can = !targets.is_empty();
                            let button = ui.add_enabled(can, egui::Button::new("Send"));
                            if !can {
                                hint(ui, "Tick at least one device.");
                            }
                            if button.clicked() {
                                self.send(targets.clone());
                            }
                        }
                        if let Some(e) = &self.error {
                            ui.colored_label(RED, e);
                        }
                    });
                    self.transfers(ui, &transfers);
                });
            });
        self.selected = selected;
    }
}

pub fn run_send(config_path: PathBuf, initial: Vec<PathBuf>) -> anyhow::Result<()> {
    let options = window_options("Send files – ClipBridge", [420.0, 460.0], [340.0, 300.0]);
    eframe::run_native(
        "ClipBridge Send",
        options,
        Box::new(move |cc| {
            let shared = Arc::new(Mutex::new(Shared::default()));
            spawn_poller(
                cc.egui_ctx.clone(),
                config_path.clone(),
                Watcher::Send,
                shared.clone(),
            );
            Ok(Box::new(SendApp {
                config_path,
                staged: initial,
                selected: HashSet::new(),
                error: None,
                focused: false,
                shared,
            }))
        }),
    )
    .map_err(|e| anyhow::anyhow!("send window: {e}"))
}

// ---------------------------------------------------------------------------------
// Incoming window

struct IncomingApp {
    config_path: PathBuf,
    /// Offers already brought to the user's attention.
    seen: HashSet<u64>,
    /// Offers on screen right now; closing the window declines them.
    asking: Vec<u64>,
    shared: Arc<Mutex<Shared>>,
}

impl IncomingApp {
    fn decide(&self, id: u64, accept: bool) {
        let _ = request(&self.config_path, Command::Decide { id, accept });
    }

    fn row(&self, ui: &mut egui::Ui, t: &TransferInfo) {
        match &t.state {
            TransferState::Asking => {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(&t.peer).strong());
                    ui.label("wants to send you");
                });
                ui.label(summary(t));
                ui.horizontal(|ui| {
                    if ui.button("Accept").clicked() {
                        self.decide(t.id, true);
                    }
                    if ui.button("Decline").clicked() {
                        self.decide(t.id, false);
                    }
                });
            }
            TransferState::Running | TransferState::Connecting | TransferState::Waiting => {
                ui.label(format!("Receiving {} from {}", summary(t), t.peer));
                progress(ui, t);
                if ui.small_button("Cancel").clicked() {
                    let _ = request(&self.config_path, Command::CancelTransfer { id: t.id });
                }
            }
            TransferState::Done => {
                ui.horizontal(|ui| {
                    ui.colored_label(GREEN, "Received");
                    ui.label(format!("{} from {}", summary(t), t.peer));
                });
                if let Some(saved) = &t.saved_to
                    && ui.small_button("Show in folder").clicked()
                {
                    show_in_folder(saved);
                }
            }
            TransferState::Declined(r) => {
                ui.label(format!("{} from {}: {r}", t.label, t.peer));
            }
            TransferState::Failed(r) => {
                ui.colored_label(RED, format!("{} from {}: {r}", t.label, t.peer));
            }
            TransferState::Cancelled => {
                ui.label(format!("{} from {}: cancelled", t.label, t.peer));
            }
        }
    }
}

impl eframe::App for IncomingApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let (status, transfers) = {
            let s = lock(&self.shared);
            (s.status.clone(), s.transfers.clone())
        };
        let incoming: Vec<TransferInfo> = transfers
            .into_iter()
            .filter(|t| !t.outgoing)
            .rev()
            .collect();
        self.asking = incoming
            .iter()
            .filter(|t| t.state == TransferState::Asking)
            .map(|t| t.id)
            .collect();
        // A new question: bring the window up.
        if self.asking.iter().any(|id| !self.seen.contains(id)) {
            self.seen.extend(self.asking.iter().copied());
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Focus);
        }

        egui::CentralPanel::default_margins()
            .frame(egui::Frame::central_panel(ui.style()))
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink(false)
                    .show(ui, |ui| {
                        if let Some(Err(_)) = &status {
                            not_running(ui);
                        }
                        if incoming.is_empty() {
                            ui.label("Nothing incoming.");
                            return;
                        }
                        for t in &incoming {
                            card(ui, |ui| self.row(ui, t));
                            ui.add_space(4.0);
                        }
                        if incoming.iter().any(|t| t.state.finished())
                            && ui.button("Clear finished").clicked()
                        {
                            let _ = request(
                                &self.config_path,
                                Command::ClearFinished { outgoing: false },
                            );
                        }
                        hint(
                            ui,
                            "Closing this window declines offers you haven't answered.",
                        );
                    });
            });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        for id in std::mem::take(&mut self.asking) {
            self.decide(id, false);
        }
    }
}

pub fn run_incoming(config_path: PathBuf) -> anyhow::Result<()> {
    let options = window_options(
        "Incoming files – ClipBridge",
        [420.0, 320.0],
        [320.0, 200.0],
    );
    eframe::run_native(
        "ClipBridge Incoming",
        options,
        Box::new(move |cc| {
            let shared = Arc::new(Mutex::new(Shared::default()));
            spawn_poller(
                cc.egui_ctx.clone(),
                config_path.clone(),
                Watcher::Incoming,
                shared.clone(),
            );
            Ok(Box::new(IncomingApp {
                config_path,
                seen: HashSet::new(),
                asking: Vec::new(),
                shared,
            }))
        }),
    )
    .map_err(|e| anyhow::anyhow!("incoming window: {e}"))
}

#[cfg(test)]
mod tests {
    use super::fmt_bytes;

    #[test]
    fn bytes_are_readable() {
        assert_eq!(fmt_bytes(5), "5 B");
        assert_eq!(fmt_bytes(2048), "2.0 KB");
        assert_eq!(fmt_bytes(48 * 1024 * 1024), "48 MB");
        assert_eq!(fmt_bytes(3 * 1024 * 1024 * 1024 / 2), "1.5 GB");
    }
}
