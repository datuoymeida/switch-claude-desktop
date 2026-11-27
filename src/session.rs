//! Capturing and restoring the files that make up a Claude Desktop login session.
//!
//! Claude Desktop is an Electron app: "being logged in" means having the claude.ai cookies
//! and web storage of that account in the app's data directory. A saved account is a copy
//! of those entries; switching accounts puts another copy in their place while the app is
//! not running. Caches, downloaded runtimes, extensions and settings are shared by all
//! accounts and are never touched.
//!
//! Cookie values are encrypted with a key held by the OS (Keychain on macOS, DPAPI via
//! `Local State` on Windows). The copies stay encrypted with that same key, so they are
//! only usable on this machine and by this user, exactly like the originals.

use anyhow::{Context, Result, ensure};
use std::fs;
use std::path::{Path, PathBuf};

use crate::fsutil::{copy_any, remove_any};

/// Entries of the data directory that hold the signed-in account's browser state,
/// relative to the data directory and written with `/` separators. Each one is captured
/// when it exists. The cookie store lives under `Network/` on Windows.
pub const SESSION_ITEMS: &[&str] = &[
    "Cookies",
    "Cookies-journal",
    "Cookies-wal",
    "Network/Cookies",
    "Network/Cookies-journal",
    "Network/Cookies-wal",
    "Local Storage",
    "Session Storage",
    "IndexedDB",
    "WebStorage",
    "Service Worker",
];

/// Scratch directory inside the data directory used while restoring.
const STAGING_DIR: &str = ".asc-restore-staging";

fn item_path(base: &Path, item: &str) -> PathBuf {
    item.split('/')
        .fold(base.to_path_buf(), |path, part| path.join(part))
}

fn sibling(path: &Path, suffix: &str) -> Result<PathBuf> {
    let mut name = path
        .file_name()
        .with_context(|| format!("Đường dẫn không hợp lệ: {}", path.display()))?
        .to_os_string();
    name.push(suffix);
    Ok(path.with_file_name(name))
}

/// Copy the session entries of `data_dir` into `dest`, replacing any previous snapshot.
/// The previous snapshot stays intact until the new one is complete.
/// Returns how many entries were captured.
pub fn capture(data_dir: &Path, dest: &Path) -> Result<usize> {
    let incoming = sibling(dest, ".new")?;
    let outgoing = sibling(dest, ".old")?;
    remove_any(&incoming)?;
    fs::create_dir_all(&incoming)
        .with_context(|| format!("Không tạo được {}", incoming.display()))?;

    let mut captured = 0;
    for item in SESSION_ITEMS {
        let source = item_path(data_dir, item);
        if source.exists() {
            copy_any(&source, &item_path(&incoming, item))?;
            captured += 1;
        }
    }

    remove_any(&outgoing)?;
    if dest.exists() {
        fs::rename(dest, &outgoing)
            .with_context(|| format!("Không thay được {}", dest.display()))?;
    }
    fs::rename(&incoming, dest).with_context(|| format!("Không ghi được {}", dest.display()))?;
    remove_any(&outgoing)?;
    Ok(captured)
}

/// Replace the session entries of `data_dir` with the snapshot in `snapshot`.
/// Claude must not be running.
pub fn restore(snapshot: &Path, data_dir: &Path) -> Result<()> {
    ensure!(
        snapshot.is_dir(),
        "Không tìm thấy dữ liệu phiên đã lưu tại {}",
        snapshot.display()
    );
    ensure!(
        data_dir.is_dir(),
        "Không tìm thấy thư mục dữ liệu của Claude tại {}",
        data_dir.display()
    );

    // Copy everything next to its destination first, so the swap below is only renames
    // and a failed copy (full disk, locked file) leaves the live session as it was.
    let staging = data_dir.join(STAGING_DIR);
    remove_any(&staging)?;
    for item in SESSION_ITEMS {
        let saved = item_path(snapshot, item);
        if saved.exists() {
            copy_any(&saved, &item_path(&staging, item))?;
        }
    }

    for item in SESSION_ITEMS {
        let live = item_path(data_dir, item);
        remove_any(&live)?;
        let staged = item_path(&staging, item);
        if staged.exists() {
            if let Some(parent) = live.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("Không tạo được {}", parent.display()))?;
            }
            fs::rename(&staged, &live)
                .with_context(|| format!("Không khôi phục được {}", live.display()))?;
        }
    }
    remove_any(&staging)
}

/// Remove the session entries of `data_dir`, leaving Claude signed out on this machine.
/// Nothing is revoked on the server: a saved copy of the session keeps working.
pub fn clear(data_dir: &Path) -> Result<()> {
    for item in SESSION_ITEMS {
        remove_any(&item_path(data_dir, item))?;
    }
    remove_any(&data_dir.join(STAGING_DIR))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(base: &Path, rel: &str, content: &str) {
        let path = item_path(base, rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn read(base: &Path, rel: &str) -> Option<String> {
        fs::read_to_string(item_path(base, rel)).ok()
    }

    /// A data directory in the macOS layout, tagged with `who`.
    fn fake_data_dir(dir: &Path, who: &str) {
        write(dir, "Cookies", &format!("cookies of {who}"));
        write(dir, "Cookies-journal", "");
        write(
            dir,
            "Local Storage/leveldb/000003.log",
            &format!("local storage of {who}"),
        );
        write(
            dir,
            "Session Storage/CURRENT",
            &format!("session storage of {who}"),
        );
        write(
            dir,
            "IndexedDB/https_claude.ai_0.indexeddb.leveldb/LOG",
            &format!("idb of {who}"),
        );
        // Shared state that must never be captured or disturbed.
        write(dir, "config.json", "{}");
        write(dir, "Cache/data_0", "shared cache");
        write(dir, "claude_desktop_config.json", "{\"mcpServers\":{}}");
    }

    #[test]
    fn capture_copies_only_session_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("Claude");
        fake_data_dir(&data, "A");
        let snap = tmp.path().join("store/files");
        fs::create_dir_all(snap.parent().unwrap()).unwrap();

        assert_eq!(capture(&data, &snap).unwrap(), 5);
        assert_eq!(read(&snap, "Cookies").as_deref(), Some("cookies of A"));
        assert_eq!(
            read(&snap, "Local Storage/leveldb/000003.log").as_deref(),
            Some("local storage of A")
        );
        assert!(!snap.join("config.json").exists());
        assert!(!snap.join("Cache").exists());
        // The source is untouched.
        assert_eq!(read(&data, "Cookies").as_deref(), Some("cookies of A"));
    }

    #[test]
    fn capture_replaces_the_previous_snapshot_completely() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("Claude");
        fake_data_dir(&data, "A");
        let snap = tmp.path().join("files");
        capture(&data, &snap).unwrap();

        // The session changes and one entry disappears.
        write(&data, "Cookies", "cookies of A, refreshed");
        remove_any(&data.join("Session Storage")).unwrap();
        capture(&data, &snap).unwrap();

        assert_eq!(
            read(&snap, "Cookies").as_deref(),
            Some("cookies of A, refreshed")
        );
        assert!(!snap.join("Session Storage").exists());
        assert!(!sibling(&snap, ".new").unwrap().exists());
        assert!(!sibling(&snap, ".old").unwrap().exists());
    }

    #[test]
    fn restore_swaps_the_session_and_keeps_shared_state() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("Claude");
        fake_data_dir(&data, "A");
        let snap_a = tmp.path().join("a");
        capture(&data, &snap_a).unwrap();

        // Account B is now live and has an entry A never had.
        fake_data_dir(&data, "B");
        write(&data, "WebStorage/QuotaManager", "quota of B");
        let snap_b = tmp.path().join("b");
        capture(&data, &snap_b).unwrap();

        restore(&snap_a, &data).unwrap();
        assert_eq!(read(&data, "Cookies").as_deref(), Some("cookies of A"));
        assert_eq!(
            read(&data, "Session Storage/CURRENT").as_deref(),
            Some("session storage of A")
        );
        // B's extra entry must not leak into A's session.
        assert!(!data.join("WebStorage").exists());
        assert_eq!(read(&data, "Cache/data_0").as_deref(), Some("shared cache"));
        assert_eq!(
            read(&data, "claude_desktop_config.json").as_deref(),
            Some("{\"mcpServers\":{}}")
        );
        assert!(!data.join(STAGING_DIR).exists());

        restore(&snap_b, &data).unwrap();
        assert_eq!(read(&data, "Cookies").as_deref(), Some("cookies of B"));
        assert_eq!(
            read(&data, "WebStorage/QuotaManager").as_deref(),
            Some("quota of B")
        );
        // Snapshots are copies: restoring does not consume them.
        assert_eq!(read(&snap_a, "Cookies").as_deref(), Some("cookies of A"));
    }

    #[test]
    fn windows_layout_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("Claude");
        write(&data, "Network/Cookies", "cookies of A");
        write(
            &data,
            "Network/Network Persistent State",
            "shared network state",
        );
        write(&data, "Local State", "{\"os_crypt\":{}}");
        let snap = tmp.path().join("a");
        assert_eq!(capture(&data, &snap).unwrap(), 1);
        assert!(
            !snap
                .join("Network")
                .join("Network Persistent State")
                .exists()
        );

        clear(&data).unwrap();
        assert!(!data.join("Network").join("Cookies").exists());
        // Neighbours of the cookie store, and the key file, survive.
        assert_eq!(
            read(&data, "Network/Network Persistent State").as_deref(),
            Some("shared network state")
        );
        assert_eq!(
            read(&data, "Local State").as_deref(),
            Some("{\"os_crypt\":{}}")
        );

        restore(&snap, &data).unwrap();
        assert_eq!(
            read(&data, "Network/Cookies").as_deref(),
            Some("cookies of A")
        );
    }

    #[test]
    fn clear_signs_out_locally() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("Claude");
        fake_data_dir(&data, "A");
        clear(&data).unwrap();
        for item in [
            "Cookies",
            "Cookies-journal",
            "Local Storage",
            "Session Storage",
            "IndexedDB",
        ] {
            assert!(!item_path(&data, item).exists(), "{item} should be gone");
        }
        assert_eq!(read(&data, "config.json").as_deref(), Some("{}"));
        // Clearing twice is harmless.
        clear(&data).unwrap();
    }

    #[test]
    fn restore_refuses_a_missing_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("Claude");
        fake_data_dir(&data, "A");
        assert!(restore(&tmp.path().join("nope"), &data).is_err());
        // The live session was not touched.
        assert_eq!(read(&data, "Cookies").as_deref(), Some("cookies of A"));
    }
}
