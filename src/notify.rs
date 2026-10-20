//! System notifications (Windows toasts, macOS Notification Center). They are shown by
//! a short-lived helper process (`--notify <title> <body>`), so the background process
//! never loads the notification frameworks.

/// Matches the macOS bundle id and the AppUserModelID on the installer's shortcut.
const APP_ID: &str = "dev.clipbridge.ClipBridge";

/// Shows a notification from a helper process; returns at once.
pub fn spawn(title: &str, body: &str) {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("cannot locate executable: {e}");
            return;
        }
    };
    if let Err(e) = std::process::Command::new(exe)
        .args(["--notify", title, body])
        .spawn()
    {
        tracing::warn!("cannot show notification: {e}");
    }
}

/// Runs in the helper process.
pub fn show(title: &str, body: &str) -> anyhow::Result<()> {
    let mut n = notify_rust::Notification::new();
    n.summary(title).body(body);
    // Toasts need a registered app id; only the installed copy has one (its Start
    // menu shortcut). Elsewhere notify-rust falls back to PowerShell's.
    #[cfg(target_os = "windows")]
    if crate::update::installed() {
        n.app_id(APP_ID);
    }
    // Attribute the notification to our bundle; a bare binary keeps the default.
    #[cfg(target_os = "macos")]
    if crate::update::installed() {
        let _ = notify_rust::set_application(APP_ID);
    }
    let _ = APP_ID;
    n.show()
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("notification: {e}"))
}
