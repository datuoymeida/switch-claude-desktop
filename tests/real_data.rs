//! A rehearsal on a *copy* of this machine's real Claude Desktop data.
//!
//!     cargo test --test real_data -- --ignored --nocapture
//!
//! The session entries and config of the real data directory are copied into a temporary
//! sandbox; "save, sign out, switch back" then runs against the sandbox, and the session
//! must come back byte for byte. The real data directory is only ever read, Claude is
//! never quit or started, and the sandbox is deleted afterwards.
//!
//! Worth running on an installation this tool has not seen before (a new Claude version,
//! a different Windows install type) before trusting it with the real session.

use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use app_switch_claude::claude_app::{ClaudeControl, QuitError, QuitMode};
use app_switch_claude::store::Store;
use app_switch_claude::switcher::{LiveSession, SaveOptions, Switcher};
use app_switch_claude::{config_json, fsutil, paths, session};

/// A Claude that is not running and never needs to be.
struct StoppedClaude;

impl ClaudeControl for StoppedClaude {
    fn is_running(&self) -> bool {
        false
    }
    fn quit(&self, _mode: QuitMode) -> Result<(), QuitError> {
        Ok(())
    }
    fn launch(&self) -> anyhow::Result<()> {
        Ok(())
    }
    fn flushes_on_quit(&self) -> bool {
        true
    }
}

fn join(base: &Path, item: &str) -> PathBuf {
    item.split('/')
        .fold(base.to_path_buf(), |path, part| path.join(part))
}

/// Size and SHA-256 of every file below the session entries of `data_dir`.
fn fingerprint(data_dir: &Path) -> BTreeMap<String, (u64, String)> {
    fn walk(path: &Path, name: String, out: &mut BTreeMap<String, (u64, String)>) {
        if path.is_dir() {
            for entry in fs::read_dir(path).unwrap() {
                let entry = entry.unwrap();
                let child = format!("{name}/{}", entry.file_name().to_string_lossy());
                walk(&entry.path(), child, out);
            }
        } else if path.is_file() {
            let bytes = fs::read(path).unwrap();
            let hash: String = Sha256::digest(&bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            out.insert(name, (bytes.len() as u64, hash));
        }
    }
    let mut out = BTreeMap::new();
    for item in session::SESSION_ITEMS {
        walk(&join(data_dir, item), (*item).to_owned(), &mut out);
    }
    out
}

#[test]
#[ignore = "reads this machine's real Claude Desktop data (and never writes to it)"]
fn rehearsal_on_a_copy_of_the_real_data() {
    let Some(real) = paths::claude_data_dir().filter(|dir| dir.is_dir()) else {
        eprintln!("No Claude Desktop data directory on this machine; nothing to rehearse.");
        return;
    };
    eprintln!("Real data directory (read only): {}", real.display());

    let tmp = tempfile::tempdir().unwrap();
    let sandbox = tmp.path().join("Claude");
    fs::create_dir_all(&sandbox).unwrap();
    let shared = ["config.json", "config.json.journal", "Local State"];
    for item in session::SESSION_ITEMS.iter().copied().chain(shared) {
        let source = join(&real, item);
        if source.exists() {
            fsutil::copy_any(&source, &join(&sandbox, item)).unwrap();
        }
    }

    let files_before = fingerprint(&sandbox);
    let config_before = config_json::read_effective(&sandbox)
        .unwrap()
        .unwrap_or_default();
    let account_keys_before = config_json::account_keys(&config_before);
    let bytes: u64 = files_before.values().map(|(len, _)| len).sum();
    eprintln!(
        "Copied {} session files ({} KiB) and {} per-account config entries into a sandbox.",
        files_before.len(),
        bytes / 1024,
        account_keys_before.len()
    );

    let switcher = Switcher::new(
        Store::open(tmp.path().join("store")).unwrap(),
        Some(sandbox.clone()),
        Box::new(StoppedClaude),
    );
    let say = |text: &str| eprintln!("  · {text}");

    let live = switcher.overview().unwrap().live;
    let description = match &live {
        LiveSession::Unsaved { .. } => "logged in (not saved yet)",
        LiveSession::LoggedOut => "not logged in",
        _ => "unexpected",
    };
    eprintln!("Claude's session: {description}");

    // 1. Save the session as an account.
    let options = SaveOptions {
        allow_unverified: true,
        ..SaveOptions::default()
    };
    eprintln!(
        "{}",
        switcher.save_current("Rehearsal", options, &say).unwrap()
    );
    let id = switcher.overview().unwrap().accounts[0].account.id.clone();
    assert_eq!(
        fingerprint(&sandbox),
        files_before,
        "saving must not change the live session"
    );

    // 2. Sign out locally, as "add another account" does.
    eprintln!("{}", switcher.prepare_new_login(false, &say).unwrap());
    assert!(
        fingerprint(&sandbox).is_empty(),
        "every session file should be gone"
    );
    let signed_out = config_json::read_effective(&sandbox)
        .unwrap()
        .unwrap_or_default();
    assert!(config_json::account_keys(&signed_out).is_empty());
    for (key, value) in &config_before {
        if !config_json::is_account_key(key) {
            assert_eq!(
                signed_out.get(key),
                Some(value),
                "setting {key} must be preserved"
            );
        }
    }
    assert_eq!(switcher.overview().unwrap().live, LiveSession::LoggedOut);

    // 3. Switch back: everything returns exactly.
    eprintln!("{}", switcher.switch_to(&id, false, &say).unwrap());
    assert_eq!(
        fingerprint(&sandbox),
        files_before,
        "the session must be restored byte for byte"
    );
    let config_after = config_json::read_effective(&sandbox)
        .unwrap()
        .unwrap_or_default();
    assert_eq!(
        config_after, config_before,
        "config.json must hold the same values as before"
    );
    if live != LiveSession::LoggedOut {
        assert_eq!(switcher.overview().unwrap().live, LiveSession::Saved { id });
    }
    assert!(
        !sandbox.join("config.json.journal").exists(),
        "no journal may be left to override the restored config"
    );
    // The key that decrypts the cookies is never touched.
    if real.join("Local State").exists() {
        assert_eq!(
            fs::read(sandbox.join("Local State")).unwrap(),
            fs::read(real.join("Local State")).unwrap()
        );
    }
    eprintln!(
        "OK: {} files and {} config entries came back identical.",
        files_before.len(),
        account_keys_before.len()
    );
}
