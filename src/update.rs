//! Auto-update from GitHub Releases.
//!
//! Each release carries `manifest.json` (version + blake3 of every file) and
//! `manifest.json.sig`, an ed25519 signature made in CI with a key whose public half
//! is embedded below. We fetch the manifest of the latest release, verify the
//! signature, download the file for this platform, check its hash, then:
//! - Windows (installed copy): run the installer silently; it replaces and relaunches us.
//! - macOS (.app bundle): swap the bundle in place and relaunch.
//! - Anything else (portable exe, bare binary): only report that an update exists.
//!
//! Everything here is blocking; callers run it on a blocking thread.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

const REPO: &str = "VeiaG/clipbridge";
/// Public half of the release signing key (`tools/release-sign`).
const PUBLIC_KEY: &str = "5aa1012211724b1d76c5c11179e068a2410b5a627f9ff9a6f39b5f0d9e69fbdd";
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_ASSET_BYTES: u64 = 200 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asset {
    pub name: String,
    pub blake3: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Manifest {
    pub version: String,
    pub assets: BTreeMap<String, Asset>,
}

/// An available update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateInfo {
    pub version: String,
    /// `None` when this copy can't update itself (portable exe, bare binary).
    pub asset: Option<Asset>,
    pub release_url: String,
}

impl UpdateInfo {
    pub fn can_install(&self) -> bool {
        self.asset.is_some()
    }
}

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let mut it = v.trim().trim_start_matches('v').split('.');
    let mut next = || it.next()?.split(['-', '+']).next()?.parse().ok();
    Some((next()?, next()?, next()?))
}

pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(c), Some(cur)) => c > cur,
        _ => false,
    }
}

fn unhex<const N: usize>(s: &str) -> Result<[u8; N]> {
    let s = s.trim();
    if s.len() != N * 2 {
        bail!("expected {} hex characters", N * 2);
    }
    let mut out = [0u8; N];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).context("bad hex")?;
    }
    Ok(out)
}

/// Checks the manifest signature against `public_key` and parses it.
pub fn verify_manifest(body: &[u8], sig_hex: &str, public_key: &str) -> Result<Manifest> {
    let key = ed25519_dalek::VerifyingKey::from_bytes(&unhex::<32>(public_key)?)
        .map_err(|e| anyhow!("bad public key: {e}"))?;
    let sig = ed25519_dalek::Signature::from_bytes(&unhex::<64>(sig_hex)?);
    key.verify_strict(body, &sig)
        .map_err(|_| anyhow!("update manifest signature is invalid"))?;
    serde_json::from_slice(body).context("bad update manifest")
}

// ---------------------------------------------------------------------------------
// Where this copy lives, and what it can install

#[derive(Debug, Clone, PartialEq, Eq)]
enum InstallKind {
    WindowsInstaller,
    MacBundle(PathBuf),
    Manual,
}

/// Running from the Windows installer's folder or a macOS `.app` bundle.
pub fn installed() -> bool {
    !matches!(install_kind(), InstallKind::Manual)
}

fn install_kind() -> InstallKind {
    let Ok(exe) = std::env::current_exe() else {
        return InstallKind::Manual;
    };
    install_kind_for(&exe)
}

fn install_kind_for(exe: &Path) -> InstallKind {
    if cfg!(target_os = "windows") {
        // The installer puts us in %LOCALAPPDATA%\Programs\ClipBridge.
        let installed = std::env::var_os("LOCALAPPDATA")
            .map(|d| Path::new(&d).join("Programs").join("ClipBridge"))
            .is_some_and(|dir| exe.parent() == Some(dir.as_path()));
        if installed {
            return InstallKind::WindowsInstaller;
        }
    }
    if cfg!(target_os = "macos") {
        // .../ClipBridge.app/Contents/MacOS/clipbridge
        let bundle = exe.parent().and_then(Path::parent).and_then(Path::parent);
        if let Some(b) = bundle
            && b.extension().is_some_and(|e| e == "app")
        {
            return InstallKind::MacBundle(b.to_path_buf());
        }
    }
    InstallKind::Manual
}

fn asset_key(kind: &InstallKind) -> Option<&'static str> {
    match kind {
        InstallKind::WindowsInstaller => Some("windows-x64-setup"),
        InstallKind::MacBundle(_) => Some("macos-arm64-dmg"),
        InstallKind::Manual => None,
    }
}

// ---------------------------------------------------------------------------------
// Network

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .user_agent(format!("ClipBridge/{}", current_version()))
        .build()
        .into()
}

fn fetch(agent: &ureq::Agent, url: &str, limit: u64) -> Result<Vec<u8>> {
    agent
        .get(url)
        .call()
        .with_context(|| format!("GET {url}"))?
        .body_mut()
        .with_config()
        .limit(limit)
        .read_to_vec()
        .with_context(|| format!("read {url}"))
}

/// Looks for a newer release. `Ok(None)` when up to date.
pub fn check() -> Result<Option<UpdateInfo>> {
    let agent = agent(Duration::from_secs(30));
    let base = format!("https://github.com/{REPO}/releases/latest/download");
    let body = fetch(&agent, &format!("{base}/manifest.json"), MAX_MANIFEST_BYTES)?;
    let sig = fetch(&agent, &format!("{base}/manifest.json.sig"), 1024)?;
    let manifest = verify_manifest(&body, &String::from_utf8_lossy(&sig), PUBLIC_KEY)?;
    if !is_newer(&manifest.version, current_version()) {
        return Ok(None);
    }
    let kind = install_kind();
    let asset = asset_key(&kind).and_then(|k| manifest.assets.get(k).cloned());
    Ok(Some(UpdateInfo {
        release_url: format!(
            "https://github.com/{REPO}/releases/tag/v{}",
            manifest.version
        ),
        version: manifest.version,
        asset,
    }))
}

fn download(info: &UpdateInfo, asset: &Asset) -> Result<PathBuf> {
    let url = format!(
        "https://github.com/{REPO}/releases/download/v{}/{}",
        info.version, asset.name
    );
    let bytes = fetch(&agent(Duration::from_secs(600)), &url, MAX_ASSET_BYTES)?;
    if bytes.len() as u64 != asset.size || blake3::hash(&bytes).to_hex().as_str() != asset.blake3 {
        bail!("downloaded file does not match the signed manifest");
    }
    let dir = std::env::temp_dir().join(format!("clipbridge-update-{}", info.version));
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(&asset.name);
    std::fs::write(&path, bytes).with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

/// Downloads, verifies and installs the update. On success the caller must quit
/// promptly: the installer (Windows) or a relaunch helper (macOS) takes over.
pub fn install(info: &UpdateInfo) -> Result<()> {
    let asset = info.asset.as_ref().ok_or_else(|| {
        anyhow!("this copy can't update itself; download it from the release page")
    })?;
    let kind = install_kind();
    let file = download(info, asset)?;
    tracing::info!("update {} downloaded and verified", info.version);
    match kind {
        InstallKind::WindowsInstaller => {
            // The installer closes us, replaces the exe and starts the new version.
            std::process::Command::new(&file)
                .args(["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/SP-"])
                .spawn()
                .context("start installer")?;
            Ok(())
        }
        InstallKind::MacBundle(bundle) => install_mac(&file, &bundle),
        InstallKind::Manual => bail!("this copy can't update itself"),
    }
}

fn run(cmd: &str, args: &[&str]) -> Result<()> {
    let status = std::process::Command::new(cmd)
        .args(args)
        .status()
        .with_context(|| format!("run {cmd}"))?;
    if !status.success() {
        bail!("{cmd} failed ({status})");
    }
    Ok(())
}

/// Mounts the DMG, swaps the .app bundle in place and schedules a relaunch once we exit.
fn install_mac(dmg: &Path, bundle: &Path) -> Result<()> {
    let dmg_s = dmg.to_string_lossy();
    let mount = dmg.with_extension("mount");
    let mount_s = mount.to_string_lossy().into_owned();
    std::fs::create_dir_all(&mount)?;
    run(
        "hdiutil",
        &[
            "attach",
            "-nobrowse",
            "-quiet",
            "-mountpoint",
            &mount_s,
            &dmg_s,
        ],
    )?;
    let result = (|| {
        let src = mount.join("ClipBridge.app");
        let staged = bundle.with_extension("app.new");
        let old = bundle.with_extension("app.old");
        let _ = std::fs::remove_dir_all(&staged);
        let _ = std::fs::remove_dir_all(&old);
        // ditto keeps the bundle intact (symlinks, xattrs, the ad-hoc signature).
        run(
            "ditto",
            &[&src.to_string_lossy(), &staged.to_string_lossy()],
        )?;
        std::fs::rename(bundle, &old).context("move old app aside (is /Applications writable?)")?;
        if let Err(e) = std::fs::rename(&staged, bundle) {
            let _ = std::fs::rename(&old, bundle);
            return Err(anyhow!("install new app: {e}"));
        }
        let _ = std::fs::remove_dir_all(&old);
        Ok(())
    })();
    let _ = run("hdiutil", &["detach", "-quiet", &mount_s]);
    result?;

    // Relaunch after this process exits (a second instance would just defer to us).
    let script = format!(
        "while kill -0 {pid} 2>/dev/null; do sleep 0.2; done; open \"$0\"",
        pid = std::process::id()
    );
    std::process::Command::new("/bin/sh")
        .args(["-c", &script, &bundle.to_string_lossy()])
        .spawn()
        .context("schedule relaunch")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_ordering() {
        assert!(is_newer("0.2.2", "0.2.1"));
        assert!(is_newer("v1.0.0", "0.9.9"));
        assert!(is_newer("0.10.0", "0.9.0"));
        assert!(!is_newer("0.2.1", "0.2.1"));
        assert!(!is_newer("0.2.0", "0.2.1"));
        assert!(!is_newer("garbage", "0.2.1"));
        assert_eq!(parse_version("1.2.3-beta"), Some((1, 2, 3)));
    }

    #[test]
    fn manifest_signature_is_enforced() {
        use ed25519_dalek::{Signer, SigningKey};
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let public: String = key
            .verifying_key()
            .to_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let body = br#"{"version":"9.9.9","assets":{"macos-arm64-dmg":{"name":"x.dmg","blake3":"ab","size":1}}}"#;
        let sig: String = key
            .sign(body)
            .to_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();

        let m = verify_manifest(body, &sig, &public).unwrap();
        assert_eq!(m.version, "9.9.9");
        assert_eq!(m.assets["macos-arm64-dmg"].name, "x.dmg");

        let mut tampered = body.to_vec();
        tampered[13] = b'8';
        assert!(verify_manifest(&tampered, &sig, &public).is_err());
        let other = SigningKey::from_bytes(&[8u8; 32]);
        let other_pub: String = other
            .verifying_key()
            .to_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert!(verify_manifest(body, &sig, &other_pub).is_err());
        assert!(verify_manifest(body, "zz", &public).is_err());
    }

    #[test]
    fn embedded_key_is_valid() {
        assert!(ed25519_dalek::VerifyingKey::from_bytes(&unhex::<32>(PUBLIC_KEY).unwrap()).is_ok());
    }

    #[test]
    fn install_kind_detection() {
        if cfg!(target_os = "macos") {
            let exe = Path::new("/Applications/ClipBridge.app/Contents/MacOS/clipbridge");
            assert_eq!(
                install_kind_for(exe),
                InstallKind::MacBundle(PathBuf::from("/Applications/ClipBridge.app"))
            );
        }
        assert_eq!(
            install_kind_for(Path::new("/tmp/clipbridge")),
            InstallKind::Manual
        );
    }
}

#[cfg(test)]
mod live {
    /// Hits GitHub. Run manually: cargo test live_update_check -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_update_check() {
        println!("{:?}", super::check());
    }
}
