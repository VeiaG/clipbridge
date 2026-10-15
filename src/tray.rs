//! Tray icon (Windows notification area / macOS menu bar) on the main-thread event loop.

use std::path::{Path, PathBuf};
use std::process::Child;

use anyhow::{Context, Result};
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoop, EventLoopProxy};
use tokio::sync::mpsc;
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::app::{AppEvent, AppStatus, State, UiCmd};

#[derive(Debug)]
pub enum UserEvent {
    App(AppEvent),
    Menu(MenuEvent),
    Tray(TrayIconEvent),
    /// The async runtime finished shutting down.
    Exited,
}

pub struct TrayOptions {
    pub config_path: PathBuf,
    pub log_dir: PathBuf,
    /// Extra args to pass to child processes / autostart (e.g. a custom `--config`).
    pub passthrough_args: Vec<String>,
}

// ---------------------------------------------------------------------------------
// Icons, drawn in code: a clipboard silhouette whose colour shows the state.

const ICON_SIZE: u32 = 32;

fn state_color(state: State) -> [u8; 3] {
    match state {
        State::Active => [0x2e, 0xa0, 0x5b],
        State::Paused => [0x8a, 0x8f, 0x98],
        State::NotConfigured | State::Starting => [0xe0, 0x9a, 0x2a],
        State::Error => [0xd6, 0x45, 0x45],
    }
}

fn icon_rgba(state: State) -> Vec<u8> {
    let s = ICON_SIZE as i32;
    let [r, g, b] = state_color(state);
    let dark = [r / 2, g / 2, b / 2];
    let mut px = vec![0u8; (s * s * 4) as usize];
    let mut put = |x: i32, y: i32, c: [u8; 3]| {
        let i = ((y * s + x) * 4) as usize;
        px[i..i + 4].copy_from_slice(&[c[0], c[1], c[2], 255]);
    };
    let in_rounded = |x: i32, y: i32, x0: i32, y0: i32, x1: i32, y1: i32, rad: i32| {
        if x < x0 || x > x1 || y < y0 || y > y1 {
            return false;
        }
        let cx = x.clamp(x0 + rad, x1 - rad);
        let cy = y.clamp(y0 + rad, y1 - rad);
        (x - cx).pow(2) + (y - cy).pow(2) <= rad * rad
    };
    for y in 0..s {
        for x in 0..s {
            if in_rounded(x, y, 11, 1, 20, 7, 2) {
                put(x, y, dark); // clip
            } else if in_rounded(x, y, 4, 4, 27, 31, 4) {
                let line = (9..=22).contains(&x) && matches!(y, 12..=13 | 18..=19 | 24..=25);
                let paused_bars = state == State::Paused
                    && (11..=24).contains(&y)
                    && matches!(x, 11..=13 | 18..=20);
                let white = if state == State::Paused {
                    paused_bars
                } else if state == State::Active {
                    line
                } else {
                    // "!" mark
                    (15..=16).contains(&x) && (matches!(y, 10..=20) || matches!(y, 24..=25))
                };
                put(x, y, if white { [255, 255, 255] } else { [r, g, b] });
            }
        }
    }
    px
}

fn icon(state: State) -> Option<Icon> {
    Icon::from_rgba(icon_rgba(state), ICON_SIZE, ICON_SIZE)
        .map_err(|e| tracing::warn!("tray icon: {e}"))
        .ok()
}

// ---------------------------------------------------------------------------------
// Helpers

pub fn open_path(path: &Path, as_text: bool) {
    #[cfg(target_os = "windows")]
    let mut cmd = if as_text {
        let mut c = std::process::Command::new("notepad.exe");
        c.arg(path);
        c
    } else {
        let mut c = std::process::Command::new("explorer.exe");
        c.arg(path);
        c
    };
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = std::process::Command::new("open");
        if as_text {
            c.arg("-t");
        }
        c.arg(path);
        c
    };
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let mut cmd = {
        let _ = as_text;
        let mut c = std::process::Command::new("xdg-open");
        c.arg(path);
        c
    };
    if let Err(e) = cmd.spawn() {
        tracing::warn!("cannot open {}: {e}", path.display());
    }
}

fn auto_launch(args: &[String]) -> Result<auto_launch::AutoLaunch> {
    let exe = std::env::current_exe().context("locate executable")?;
    let mut b = auto_launch::AutoLaunchBuilder::new();
    b.set_app_name("ClipBridge")
        .set_app_path(&exe.to_string_lossy())
        .set_args(args);
    #[cfg(target_os = "macos")]
    b.set_macos_launch_mode(auto_launch::MacOSLaunchMode::LaunchAgent);
    b.build().context("autostart setup")
}

pub fn autostart_enabled(args: &[String]) -> bool {
    auto_launch(args)
        .and_then(|a| a.is_enabled().context("query autostart"))
        .unwrap_or(false)
}

pub fn set_autostart(args: &[String], enable: bool) -> Result<()> {
    let a = auto_launch(args)?;
    if enable {
        a.enable().context("enable autostart")
    } else {
        a.disable().context("disable autostart")
    }
}

struct SettingsWindow {
    child: Option<Child>,
}

impl SettingsWindow {
    fn open(&mut self, passthrough: &[String]) {
        if let Some(c) = &mut self.child
            && matches!(c.try_wait(), Ok(None))
        {
            return; // already open
        }
        let exe = match std::env::current_exe() {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("cannot locate executable: {e}");
                return;
            }
        };
        match std::process::Command::new(exe)
            .arg("--settings")
            .args(passthrough)
            .spawn()
        {
            Ok(c) => self.child = Some(c),
            Err(e) => tracing::warn!("cannot open settings window: {e}"),
        }
    }
}

// ---------------------------------------------------------------------------------
// Event loop

struct Items {
    status: MenuItem,
    pause: CheckMenuItem,
    settings: MenuItem,
    open_config: MenuItem,
    open_logs: MenuItem,
    autostart: CheckMenuItem,
    quit: MenuItem,
}

fn build_menu(autostart: bool) -> Result<(Menu, Items)> {
    let items = Items {
        status: MenuItem::new("ClipBridge: starting…", false, None),
        pause: CheckMenuItem::new("Pause sync", true, false, None),
        settings: MenuItem::new("Settings…", true, None),
        open_config: MenuItem::new("Open config file", true, None),
        open_logs: MenuItem::new("Open logs folder", true, None),
        autostart: CheckMenuItem::new("Start at login", true, autostart, None),
        quit: MenuItem::new("Quit ClipBridge", true, None),
    };
    let menu = Menu::new();
    menu.append_items(&[
        &items.status,
        &PredefinedMenuItem::separator(),
        &items.pause,
        &items.settings,
        &PredefinedMenuItem::separator(),
        &items.open_config,
        &items.open_logs,
        &items.autostart,
        &PredefinedMenuItem::separator(),
        &items.quit,
    ])
    .context("build tray menu")?;
    Ok((menu, items))
}

pub fn install_event_handlers(proxy: &EventLoopProxy<UserEvent>) {
    let p = proxy.clone();
    MenuEvent::set_event_handler(Some(move |e| {
        let _ = p.send_event(UserEvent::Menu(e));
    }));
    let p = proxy.clone();
    TrayIconEvent::set_event_handler(Some(move |e| {
        let _ = p.send_event(UserEvent::Tray(e));
    }));
}

/// Runs the tray on the current (main) thread. Never returns.
pub fn run(event_loop: EventLoop<UserEvent>, ui: mpsc::Sender<UiCmd>, opts: TrayOptions) -> ! {
    let mut tray: Option<(TrayIcon, Items)> = None;
    let mut settings = SettingsWindow { child: None };
    let mut last_status: Option<AppStatus> = None;
    let mut quitting = false;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            // The tray must be created once the loop runs (required on macOS).
            Event::NewEvents(StartCause::Init) => {
                let autostart = autostart_enabled(&opts.passthrough_args);
                let built = build_menu(autostart).and_then(|(menu, items)| {
                    let mut b = TrayIconBuilder::new()
                        .with_menu(Box::new(menu))
                        .with_tooltip("ClipBridge");
                    if let Some(i) = icon(State::Starting) {
                        b = b.with_icon(i);
                    }
                    // Windows convention: left click opens the window, right click the menu.
                    #[cfg(target_os = "windows")]
                    {
                        b = b.with_menu_on_left_click(false);
                    }
                    let t = b.build().context("create tray icon")?;
                    Ok((t, items))
                });
                match built {
                    Ok(t) => tray = Some(t),
                    Err(e) => {
                        tracing::error!("{e:#}; running without a tray icon");
                    }
                }
            }
            Event::UserEvent(UserEvent::App(AppEvent::Status(status))) => {
                if let Some((icon_handle, items)) = &tray {
                    let changed_state = last_status.as_ref().map(|s| s.state) != Some(status.state);
                    if changed_state && let Some(i) = icon(status.state) {
                        let _ = icon_handle.set_icon(Some(i));
                    }
                    let mut tip = format!("ClipBridge: {}", status.summary);
                    tip.truncate(120);
                    let _ = icon_handle.set_tooltip(Some(tip));
                    let mut text = status.summary.clone();
                    if text.chars().count() > 60 {
                        text = text.chars().take(57).collect::<String>() + "…";
                    }
                    items.status.set_text(text);
                    items.pause.set_checked(status.state == State::Paused);
                }
                last_status = Some(*status);
            }
            Event::UserEvent(UserEvent::App(AppEvent::OpenSettings)) => {
                settings.open(&opts.passthrough_args);
            }
            Event::UserEvent(UserEvent::Tray(TrayIconEvent::Click {
                button: tray_icon::MouseButton::Left,
                button_state: tray_icon::MouseButtonState::Up,
                ..
            })) => {
                #[cfg(target_os = "windows")]
                settings.open(&opts.passthrough_args);
            }
            Event::UserEvent(UserEvent::Menu(e)) => {
                let Some((_, items)) = &tray else { return };
                let id = e.id();
                if id == items.pause.id() {
                    let _ = ui.try_send(UiCmd::SetPaused(items.pause.is_checked()));
                } else if id == items.settings.id() {
                    settings.open(&opts.passthrough_args);
                } else if id == items.open_config.id() {
                    open_path(&opts.config_path, true);
                } else if id == items.open_logs.id() {
                    let _ = std::fs::create_dir_all(&opts.log_dir);
                    open_path(&opts.log_dir, false);
                } else if id == items.autostart.id() {
                    let want = items.autostart.is_checked();
                    if let Err(e) = set_autostart(&opts.passthrough_args, want) {
                        tracing::warn!("{e:#}");
                        items.autostart.set_checked(!want);
                    } else {
                        tracing::info!(
                            "start at login {}",
                            if want { "enabled" } else { "disabled" }
                        );
                    }
                } else if id == items.quit.id() && !quitting {
                    quitting = true;
                    if ui.try_send(UiCmd::Quit).is_err() {
                        *control_flow = ControlFlow::Exit;
                    }
                }
            }
            Event::UserEvent(UserEvent::Exited) => {
                tray = None; // removes the icon before exit
                *control_flow = ControlFlow::Exit;
            }
            _ => {}
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icons_differ_per_state() {
        let states = [
            State::Active,
            State::Paused,
            State::NotConfigured,
            State::Error,
        ];
        let icons: Vec<_> = states.iter().map(|s| icon_rgba(*s)).collect();
        for (i, a) in icons.iter().enumerate() {
            assert_eq!(a.len(), (ICON_SIZE * ICON_SIZE * 4) as usize);
            for b in &icons[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }
}
