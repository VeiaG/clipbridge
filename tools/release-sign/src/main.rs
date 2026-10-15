//! Release signing for the ClipBridge auto-updater.
//!
//!   release-sign keygen <secret-key-file>   writes a new secret key, prints the public key
//!   release-sign manifest <dist-dir> <version>
//!       hashes the release files in <dist-dir> and writes manifest.json plus
//!       manifest.json.sig, signed with the hex secret key in $CLIPBRIDGE_SIGNING_KEY.
//!
//! The app embeds the public key and refuses any update whose manifest signature or
//! file hash does not match.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use ed25519_dalek::{Signer, SigningKey};
use serde::Serialize;

#[derive(Serialize)]
struct Asset {
    name: String,
    blake3: String,
    size: u64,
}

#[derive(Serialize)]
struct Manifest {
    version: String,
    assets: BTreeMap<String, Asset>,
}

/// File name suffix → asset key the app looks up for its platform.
const KINDS: &[(&str, &str)] = &[
    ("-windows-x64-setup.exe", "windows-x64-setup"),
    ("-windows-x64-portable.exe", "windows-x64-portable"),
    ("-macos-arm64.dmg", "macos-arm64-dmg"),
];

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex32(s: &str) -> Result<[u8; 32]> {
    let s = s.trim();
    if s.len() != 64 {
        bail!("expected 64 hex characters");
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).context("bad hex")?;
    }
    Ok(out)
}

fn keygen(path: &Path) -> Result<()> {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|e| anyhow::anyhow!("random: {e}"))?;
    std::fs::write(path, hex(&seed)).context("write secret key")?;
    let key = SigningKey::from_bytes(&seed);
    println!("{}", hex(&key.verifying_key().to_bytes()));
    Ok(())
}

fn manifest(dir: &Path, version: &str) -> Result<()> {
    let secret =
        std::env::var("CLIPBRIDGE_SIGNING_KEY").context("CLIPBRIDGE_SIGNING_KEY not set")?;
    let key = SigningKey::from_bytes(&unhex32(&secret)?);

    let mut assets = BTreeMap::new();
    for entry in std::fs::read_dir(dir).context("read dist dir")? {
        let path = entry?.path();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let Some((_, kind)) = KINDS.iter().find(|(suffix, _)| name.ends_with(suffix)) else {
            continue;
        };
        let bytes = std::fs::read(&path).with_context(|| format!("read {name}"))?;
        assets.insert(
            kind.to_string(),
            Asset {
                blake3: blake3::hash(&bytes).to_hex().to_string(),
                size: bytes.len() as u64,
                name,
            },
        );
    }
    if assets.is_empty() {
        bail!("no release files found in {}", dir.display());
    }
    let body = serde_json::to_vec_pretty(&Manifest {
        version: version.trim_start_matches('v').to_string(),
        assets,
    })?;
    let sig = key.sign(&body);
    std::fs::write(dir.join("manifest.json"), &body)?;
    std::fs::write(dir.join("manifest.json.sig"), hex(&sig.to_bytes()))?;
    println!(
        "signed manifest for {} with key {}",
        version,
        hex(&key.verifying_key().to_bytes())
    );
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["keygen", path] => keygen(Path::new(path)),
        ["manifest", dir, version] => manifest(Path::new(dir), version),
        _ => {
            bail!("usage: release-sign keygen <file> | release-sign manifest <dist-dir> <version>")
        }
    }
}
