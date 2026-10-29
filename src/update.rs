use std::{fmt::Write, io, path::PathBuf};

use anyhow::{Context, bail};
use sha2::{Digest, Sha256};

use crate::api::{Api, VersionInfo};

pub const TARGET: &str = env!("NURL_TARGET");
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[must_use]
pub fn is_older(version: &str, latest: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> {
        v.trim_start_matches('v')
            .split(['.', '-', '+'])
            .take(3)
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    };
    parse(version) < parse(latest)
}

#[must_use]
pub fn installed_by_winget() -> bool {
    std::env::current_exe().is_ok_and(|exe| {
        exe.to_string_lossy()
            .to_ascii_lowercase()
            .contains("winget")
    })
}

#[must_use]
pub fn asset_name(version: &str) -> String {
    let ext = if cfg!(windows) { ".exe" } else { "" };
    format!("nurl-{version}-{TARGET}{ext}")
}

pub async fn install(api: &Api, info: &VersionInfo) -> anyhow::Result<PathBuf> {
    let url = format!(
        "{}/agent-v{}/{}",
        info.download_url,
        info.latest,
        asset_name(&info.latest)
    );
    let binary = api
        .download(&url)
        .await
        .with_context(|| format!("не удалось скачать {url}"))?;
    let checksum = api
        .download(&format!("{url}.sha256"))
        .await
        .with_context(|| format!("не удалось скачать контрольную сумму {url}.sha256"))?;

    let expected = String::from_utf8_lossy(&checksum)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let actual = hex(&Sha256::digest(&binary));
    if expected != actual {
        bail!("контрольная сумма не совпала: файл повреждён или подменён");
    }
    replace_self(&binary).context("не удалось заменить файл агента")
}

fn replace_self(binary: &[u8]) -> io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let exe = exe.canonicalize().unwrap_or(exe);
    let fresh = exe.with_extension("new");
    std::fs::write(&fresh, binary)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fresh, std::fs::Permissions::from_mode(0o755))?;
    }
    if cfg!(windows) {
        let old = exe.with_extension("old");
        let _ = std::fs::remove_file(&old);
        std::fs::rename(&exe, &old)?;
        if let Err(e) = std::fs::rename(&fresh, &exe) {
            let _ = std::fs::rename(&old, &exe);
            return Err(e);
        }
    } else {
        std::fs::rename(&fresh, &exe)?;
    }
    Ok(exe)
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

pub fn cleanup() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(exe.with_extension("old"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert!(is_older("0.1.0", "0.2.0"));
        assert!(is_older("0.9.0", "0.10.0"));
        assert!(!is_older("0.2.0", "0.2.0"));
        assert!(!is_older("1.0.0", "0.9.9"));
    }

    #[test]
    fn asset_names_follow_release_layout() {
        let name = asset_name("1.2.3");
        assert!(name.starts_with("nurl-1.2.3-"));
        assert!(name.contains(TARGET));
    }
}
