//! Configuration: TOML file in the platform config dir.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

/// Passphrase written on first run. Sync refuses to start while it is set.
pub const PLACEHOLDER_PASSPHRASE: &str = "CHANGE-ME-to-the-same-passphrase-on-all-machines";
pub const DEFAULT_PORT: u16 = 47821;
pub const DEFAULT_MAX_IMAGE_MB: u32 = 20;
/// Frames are capped at 32 MB, so images can never be larger than this.
pub const MAX_IMAGE_MB_CAP: u32 = 30;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub device_id: String,
    pub device_name: String,
    pub passphrase: String,
    pub port: u16,
    pub manual_peers: Vec<String>,
    pub sync_images: bool,
    pub max_image_mb: u32,
    /// Advertise and browse via mDNS. Turn off on networks where multicast is blocked.
    pub discovery: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            device_id: uuid::Uuid::new_v4().to_string(),
            device_name: default_device_name(),
            passphrase: PLACEHOLDER_PASSPHRASE.to_string(),
            port: DEFAULT_PORT,
            manual_peers: Vec::new(),
            sync_images: true,
            max_image_mb: DEFAULT_MAX_IMAGE_MB,
            discovery: true,
        }
    }
}

impl Config {
    /// True once the user replaced the placeholder with a real passphrase.
    pub fn is_configured(&self) -> bool {
        let p = self.passphrase.trim();
        !p.is_empty() && p != PLACEHOLDER_PASSPHRASE
    }

    pub fn max_image_bytes(&self) -> usize {
        self.max_image_mb.clamp(1, MAX_IMAGE_MB_CAP) as usize * 1024 * 1024
    }

    pub fn parse(text: &str) -> Result<Self> {
        let mut cfg: Config = toml::from_str(text).context("invalid config TOML")?;
        if cfg.device_id.trim().is_empty() {
            cfg.device_id = uuid::Uuid::new_v4().to_string();
        }
        if cfg.device_name.trim().is_empty() {
            cfg.device_name = default_device_name();
        }
        Ok(cfg)
    }

    pub fn to_toml(&self) -> Result<String> {
        let body = toml::to_string_pretty(self).context("serialize config")?;
        Ok(format!("{HEADER}{body}"))
    }

    /// Loads the config, creating it with defaults (placeholder passphrase) if missing.
    /// Fields missing from an existing file are filled in and written back.
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if path.exists() {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("read {}", path.display()))?;
            let cfg = Self::parse(&text)?;
            if cfg.to_toml()? != text {
                // Keep the file complete so users can see every option.
                cfg.save(path)?;
            }
            Ok(cfg)
        } else {
            let cfg = Self::default();
            cfg.save(path)?;
            Ok(cfg)
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        // Write-then-rename so a reader never sees a half-written file.
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, self.to_toml()?)
            .with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))?;
        Ok(())
    }
}

const HEADER: &str = "\
# ClipBridge configuration.
# Set the SAME passphrase on every machine that should share a clipboard.
# manual_peers: extra peers as \"host:port\", for networks where mDNS discovery fails.
# Changes are picked up automatically within a few seconds.

";

fn project_dirs() -> Option<ProjectDirs> {
    ProjectDirs::from("", "", "ClipBridge")
}

pub fn default_config_path() -> PathBuf {
    project_dirs()
        .map(|d| d.config_dir().join("config.toml"))
        .unwrap_or_else(|| PathBuf::from("clipbridge.toml"))
}

pub fn default_log_dir() -> PathBuf {
    project_dirs()
        .map(|d| d.data_local_dir().join("logs"))
        .unwrap_or_else(|| PathBuf::from("logs"))
}

pub fn default_device_name() -> String {
    let from_env = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .filter(|s| !s.trim().is_empty());
    if let Some(name) = from_env {
        return name;
    }
    #[cfg(target_os = "macos")]
    if let Some(name) = command_output("scutil", &["--get", "ComputerName"]) {
        return name;
    }
    command_output("hostname", &[]).unwrap_or_else(|| "ClipBridge device".to_string())
}

fn command_output(cmd: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(cmd).args(args).output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !s.is_empty()).then_some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_not_configured() {
        let cfg = Config::default();
        assert!(!cfg.is_configured());
        assert_eq!(cfg.port, DEFAULT_PORT);
        assert!(uuid::Uuid::parse_str(&cfg.device_id).is_ok());
    }

    #[test]
    fn roundtrip_and_partial_files() {
        let cfg = Config {
            passphrase: "hunter2".into(),
            manual_peers: vec!["10.0.0.2:47821".into()],
            ..Config::default()
        };
        let parsed = Config::parse(&cfg.to_toml().unwrap()).unwrap();
        assert_eq!(parsed, cfg);
        assert!(parsed.is_configured());

        let partial = Config::parse("passphrase = \"x\"\nport = 5000\n").unwrap();
        assert_eq!(partial.port, 5000);
        assert!(partial.sync_images);
        assert_eq!(partial.max_image_mb, DEFAULT_MAX_IMAGE_MB);
        assert!(!partial.device_id.is_empty());
    }

    #[test]
    fn rejects_garbage_and_blank_passphrase() {
        assert!(Config::parse("port = \"not a number\"").is_err());
        let blank = Config::parse("passphrase = \"   \"").unwrap();
        assert!(!blank.is_configured());
    }

    #[test]
    fn image_limit_is_capped() {
        let cfg = Config {
            max_image_mb: 500,
            ..Config::default()
        };
        assert_eq!(
            cfg.max_image_bytes(),
            MAX_IMAGE_MB_CAP as usize * 1024 * 1024
        );
    }

    #[test]
    fn load_or_create_writes_file() {
        let dir = std::env::temp_dir().join(format!("clipbridge-test-{}", uuid::Uuid::new_v4()));
        let path = dir.join("config.toml");
        let created = Config::load_or_create(&path).unwrap();
        assert!(path.exists());
        let loaded = Config::load_or_create(&path).unwrap();
        assert_eq!(created, loaded);
        let _ = std::fs::remove_dir_all(dir);
    }
}
