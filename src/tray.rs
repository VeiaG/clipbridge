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
// Icons

/// Rendered larger than shown: macOS scales it to the menu bar (Retina), Windows to
/// the notification area at the current DPI.
const ICON_SIZE: u32 = 64;

/// Whether this platform/state shows a monochrome template icon (macOS menu bar).
fn is_template(state: State) -> bool {
    cfg!(target_os = "macos") && crate::icon::uses_template(state)
}

/// The icon for a state, plus whether it is a template image.
fn icon(state: State) -> Option<(Icon, bool)> {
    let template = is_template(state);
    let rgba = if cfg!(target_os = "macos") {
        // tray-icon stretches the image to the full 22pt menu-bar height; keep the
        // artwork near the ~15pt of system icons by leaving a margin.
        let art = ICON_SIZE * 2 / 3;
        let inner = if template {
            crate::icon::render_template(state, art)
        } else {
            crate::icon::render(state, art)
        };
        crate::icon::pad_center(&inner, art, ICON_SIZE)
    } else {
        crate::icon::render(state, ICON_SIZE)
    };
    Icon::from_rgba(rgba, ICON_SIZE, ICON_SIZE)
        .map_err(|e| tracing::warn!("tray icon: {e}"))
        .ok()
        .map(|i| (i, template))
}

fn builder_with_icon(b: TrayIconBuilder, icon: Icon, template: bool) -> TrayIconBuilder {
    #[cfg(target_os = "macos")]
    if template {
        return b.with_icon_templated(icon);
    }
    let _ = template;
    b.with_icon(icon)
}

fn set_tray_icon(tray: &TrayIcon, icon: Icon, template: bool) {
    #[cfg(target_os = "macos")]
    if template {
        let _ = tray.set_icon_templated(Some(icon));
        return;
    }
    let _ = template;
    let _ = tray.set_icon(Some(icon));
}

// ---------------------------------------------------------------------------------
// Helpers

/// Opens a web page in the default browser.
pub fn open_url(url: &str) {
    #[cfg(target_os = "windows")]
    let res = std::process::Command::new("rundll32.exe")
        .args(["url.dll,FileProtocolHandler", url])
        .spawn();
    #[cfg(target_os = "macos")]
    let res = std::process::Command::new("open").arg(url).spawn();
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let res = std::process::Command::new("xdg-open").arg(url).spawn();
    if let Err(e) = res {
        tracing::warn!("cannot open {url}: {e}");
    }
}

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
    // The Run registry value is a command line: quote the path in case it has spaces.
    #[cfg(target_os = "windows")]
    let path = format!("\"{}\"", exe.display());
    #[cfg(not(target_os = "windows"))]
    let path = exe.to_string_lossy().into_owned();
    let mut b = auto_launch::AutoLaunchBuilder::new();
    b.set_app_name("ClipBridge")
        .set_app_path(&path)
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

/// Windows only lets the process the user just clicked bring windows to the front.
/// Pass that right on, so the settings window doesn't open behind everything.
#[cfg(target_os = "windows")]
fn allow_child_foreground() {
    #[link(name = "user32")]
    unsafe extern "system" {
        fn AllowSetForegroundWindow(process_id: u32) -> i32;
    }
    const ASFW_ANY: u32 = u32::MAX;
    // SAFETY: plain Win32 call with no pointers; failure only means no focus change.
    unsafe {
        AllowSetForegroundWindow(ASFW_ANY);
    }
}

#[cfg(not(target_os = "windows"))]
fn allow_child_foreground() {}

struct SettingsWindow {
    child: Option<Child>,
}

impl SettingsWindow {
    fn close(&mut self) {
        if let Some(mut c) = self.child.take()
            && matches!(c.try_wait(), Ok(None))
        {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

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
        allow_child_foreground();
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
    update: MenuItem,
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
        update: MenuItem::new("Check for updates", true, None),
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
        &items.update,
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
                    if let Some((i, template)) = icon(State::Starting) {
                        b = builder_with_icon(b, i, template);
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
                    if changed_state && let Some((i, template)) = icon(status.state) {
                        set_tray_icon(icon_handle, i, template);
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
                    let u = &status.update;
                    let text = match (&u.available, u.busy) {
                        (_, true) => "Updating…".to_string(),
                        (Some(i), _) if i.can_install() => format!("Update to {}", i.version),
                        (Some(i), _) => format!("Download {}…", i.version),
                        (None, _) => "Check for updates".to_string(),
                    };
                    items.update.set_text(text);
                    items.update.set_enabled(!u.busy);
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
                } else if id == items.update.id() {
                    let _ = ui.try_send(UiCmd::Update);
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
                // Close our settings window too. On macOS this also matters for
                // updates: while it runs, `open ClipBridge.app` would just focus it
                // instead of starting the new version.
                settings.close();
                tray = None; // removes the icon before exit
                *control_flow = ControlFlow::Exit;
            }
            _ => {}
        }
    })
}

#[cfg(test)]
mod tests {
    /// Writes the real autostart entry, then removes it.
    /// Run manually: cargo test autostart -- --ignored
    #[test]
    #[ignore]
    fn autostart_roundtrip() {
        super::set_autostart(&[], true).unwrap();
        assert!(super::autostart_enabled(&[]));
        super::set_autostart(&[], false).unwrap();
        assert!(!super::autostart_enabled(&[]));
    }
}
