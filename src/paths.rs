//! Locating Claude Desktop's data directory and this tool's own storage.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Overrides the detected Claude Desktop data directory (testing, unusual installs).
pub const ENV_CLAUDE_DATA_DIR: &str = "APP_SWITCH_CLAUDE_DATA_DIR";
/// Overrides where saved accounts are kept.
pub const ENV_STORE_DIR: &str = "APP_SWITCH_CLAUDE_STORE_DIR";

const STORE_DIR_NAME: &str = "AppSwitchClaude";
const CLAUDE_DIR_NAME: &str = "Claude";

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// Where this tool keeps the account list and the saved sessions.
pub fn store_dir() -> Result<PathBuf> {
    if let Some(path) = env_path(ENV_STORE_DIR) {
        return Ok(path);
    }
    let base = dirs::data_dir().context("Không xác định được thư mục dữ liệu của người dùng")?;
    Ok(base.join(STORE_DIR_NAME))
}

/// Claude Desktop's data directory, if one exists on this machine.
pub fn claude_data_dir() -> Option<PathBuf> {
    if let Some(path) = env_path(ENV_CLAUDE_DATA_DIR) {
        return Some(path);
    }
    most_recently_used(&candidates())
}

/// Places where Claude Desktop may keep its data on this platform, most likely first.
pub fn candidates() -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        match (dirs::data_local_dir(), dirs::data_dir()) {
            (Some(local), Some(roaming)) => windows_candidates(&local, &roaming),
            (None, Some(roaming)) => vec![roaming.join(CLAUDE_DIR_NAME)],
            _ => Vec::new(),
        }
    }
    #[cfg(target_os = "macos")]
    {
        // ~/Library/Application Support/Claude
        dirs::data_dir()
            .map(|dir| vec![dir.join(CLAUDE_DIR_NAME)])
            .unwrap_or_default()
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        // Unofficial Linux builds follow the Electron default: ~/.config/Claude
        dirs::config_dir()
            .map(|dir| vec![dir.join(CLAUDE_DIR_NAME)])
            .unwrap_or_default()
    }
}

/// Candidate data directories on Windows.
///
/// The current installer is an MSIX package, whose `%APPDATA%\Claude` is redirected to
/// `%LOCALAPPDATA%\Packages\Claude_<publisher id>\LocalCache\Roaming\Claude`. The older
/// Squirrel installer used `%APPDATA%\Claude` directly, and a machine that was upgraded
/// can have both.
pub fn windows_candidates(local_app_data: &Path, roaming_app_data: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    if let Ok(entries) = std::fs::read_dir(local_app_data.join("Packages")) {
        let mut packages: Vec<PathBuf> = entries
            .flatten()
            .filter(|entry| {
                let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
                name.starts_with("claude_")
                    || (name.contains("anthropic") && name.contains("claude"))
            })
            .map(|entry| entry.path())
            .collect();
        packages.sort();
        for package in packages {
            found.push(
                package
                    .join("LocalCache")
                    .join("Roaming")
                    .join(CLAUDE_DIR_NAME),
            );
        }
    }
    found.push(roaming_app_data.join(CLAUDE_DIR_NAME));
    found
}

/// Of the candidates that exist, the one Claude wrote to most recently. Earlier candidates
/// win ties.
pub fn most_recently_used(candidates: &[PathBuf]) -> Option<PathBuf> {
    let mut best: Option<(SystemTime, &PathBuf)> = None;
    for candidate in candidates.iter().filter(|c| c.is_dir()) {
        let used = last_activity(candidate);
        if best.is_none_or(|(time, _)| used > time) {
            best = Some((used, candidate));
        }
    }
    best.map(|(_, path)| path.clone())
}

fn last_activity(data_dir: &Path) -> SystemTime {
    [
        data_dir.join("config.json"),
        data_dir.join("Cookies"),
        data_dir.join("Network").join("Cookies"),
    ]
    .iter()
    .filter_map(|file| {
        std::fs::metadata(file)
            .and_then(|meta| meta.modified())
            .ok()
    })
    .max()
    .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// The MSIX package family name (for example `Claude_pzs8sxrjxfjjc`) when `data_dir` is a
/// packaged install's redirected data directory.
pub fn msix_family_name(data_dir: &str) -> Option<String> {
    let parts: Vec<&str> = data_dir
        .split(['\\', '/'])
        .filter(|part| !part.is_empty())
        .collect();
    let n = parts.len();
    if n < 5 {
        return None;
    }
    let tail_matches = parts[n - 1].eq_ignore_ascii_case(CLAUDE_DIR_NAME)
        && parts[n - 2].eq_ignore_ascii_case("Roaming")
        && parts[n - 3].eq_ignore_ascii_case("LocalCache")
        && parts[n - 5].eq_ignore_ascii_case("Packages");
    tail_matches.then(|| parts[n - 4].to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn finds_msix_and_legacy_directories_on_windows() {
        let tmp = tempfile::tempdir().unwrap();
        let local = tmp.path().join("Local");
        let roaming = tmp.path().join("Roaming");
        fs::create_dir_all(local.join("Packages").join("Claude_pzs8sxrjxfjjc")).unwrap();
        fs::create_dir_all(
            local
                .join("Packages")
                .join("Microsoft.WindowsTerminal_8wekyb"),
        )
        .unwrap();
        fs::create_dir_all(&roaming).unwrap();

        let found = windows_candidates(&local, &roaming);
        assert_eq!(
            found,
            [
                local
                    .join("Packages")
                    .join("Claude_pzs8sxrjxfjjc")
                    .join("LocalCache")
                    .join("Roaming")
                    .join("Claude"),
                roaming.join("Claude"),
            ]
        );
        // No Packages folder at all: just the classic location.
        assert_eq!(
            windows_candidates(&tmp.path().join("missing"), &roaming),
            [roaming.join("Claude")]
        );
    }

    #[test]
    fn prefers_the_directory_claude_used_last() {
        let tmp = tempfile::tempdir().unwrap();
        let msix = tmp.path().join("msix");
        let legacy = tmp.path().join("legacy");
        let absent = tmp.path().join("absent");
        let all = [msix.clone(), legacy.clone(), absent];
        assert_eq!(most_recently_used(&all), None);

        // Only the legacy directory exists.
        fs::create_dir_all(&legacy).unwrap();
        assert_eq!(most_recently_used(&all), Some(legacy.clone()));

        // Both exist but neither has been written to: the first candidate wins.
        fs::create_dir_all(&msix).unwrap();
        assert_eq!(most_recently_used(&all), Some(msix.clone()));

        // A leftover legacy config from the past loses to a live MSIX session.
        fs::write(legacy.join("config.json"), "{}").unwrap();
        let old = SystemTime::now() - std::time::Duration::from_secs(86_400);
        fs::File::options()
            .write(true)
            .open(legacy.join("config.json"))
            .unwrap()
            .set_modified(old)
            .unwrap();
        fs::create_dir_all(msix.join("Network")).unwrap();
        fs::write(msix.join("Network").join("Cookies"), "x").unwrap();
        assert_eq!(most_recently_used(&all), Some(msix.clone()));

        // And the other way round.
        fs::write(legacy.join("Cookies"), "x").unwrap();
        let older = SystemTime::now() - std::time::Duration::from_secs(172_800);
        fs::File::options()
            .write(true)
            .open(msix.join("Network").join("Cookies"))
            .unwrap()
            .set_modified(older)
            .unwrap();
        assert_eq!(most_recently_used(&all), Some(legacy));
    }

    #[test]
    fn extracts_the_msix_family_name() {
        assert_eq!(
            msix_family_name(
                r"C:\Users\dat\AppData\Local\Packages\Claude_pzs8sxrjxfjjc\LocalCache\Roaming\Claude"
            )
            .as_deref(),
            Some("Claude_pzs8sxrjxfjjc")
        );
        assert_eq!(
            msix_family_name(
                r"C:\Users\dat\AppData\Local\Packages\Claude_pzs8sxrjxfjjc\LocalCache\Roaming\Claude\"
            )
            .as_deref(),
            Some("Claude_pzs8sxrjxfjjc")
        );
        assert_eq!(
            msix_family_name(r"C:\Users\dat\AppData\Roaming\Claude"),
            None
        );
        assert_eq!(
            msix_family_name("/Users/dat/Library/Application Support/Claude"),
            None
        );
        assert_eq!(msix_family_name(""), None);
    }
}
