//! End-to-end tests of the switching logic against a fake Claude data directory and a
//! fake Claude process. Nothing here touches a real installation.

use anyhow::bail;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use app_switch_claude::claude_app::{ClaudeControl, QuitError, QuitMode};
use app_switch_claude::config_json;
use app_switch_claude::cookies::SessionProbe;
use app_switch_claude::store::Store;
use app_switch_claude::switcher::{LiveSession, OpError, SaveOptions, Switcher};

const COOKIES_A: &[u8] = include_bytes!("fixtures/cookies_logged_in_a.db");
const COOKIES_B: &[u8] = include_bytes!("fixtures/cookies_logged_in_b.db");
const COOKIES_LOGGED_OUT: &[u8] = include_bytes!("fixtures/cookies_logged_out.db");
const COOKIES_EXPIRED: &[u8] = include_bytes!("fixtures/cookies_expired.db");

const UUID_A: &str = "aaaaaaaa-0000-4000-8000-000000000001";
const UUID_B: &str = "bbbbbbbb-0000-4000-8000-000000000002";
const UUID_C: &str = "cccccccc-0000-4000-8000-000000000003";

#[derive(Default)]
struct MockState {
    running: bool,
    /// Simulates Claude showing a "quit anyway?" dialog.
    ignores_polite_quit: bool,
    /// Simulates something restarting Claude right after it quit.
    restarts_itself: bool,
    launch_fails: bool,
    flushes_on_quit: bool,
    quits: Vec<QuitMode>,
    launches: usize,
}

#[derive(Clone)]
struct MockClaude(Arc<Mutex<MockState>>);

impl MockClaude {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(MockState {
            flushes_on_quit: true,
            ..MockState::default()
        })))
    }
    fn set(&self, change: impl FnOnce(&mut MockState)) {
        change(&mut self.0.lock().unwrap());
    }
    fn running(&self) -> bool {
        self.0.lock().unwrap().running
    }
    fn launches(&self) -> usize {
        self.0.lock().unwrap().launches
    }
    fn quits(&self) -> Vec<QuitMode> {
        self.0.lock().unwrap().quits.clone()
    }
}

impl ClaudeControl for MockClaude {
    fn is_running(&self) -> bool {
        self.0.lock().unwrap().running
    }
    fn quit(&self, mode: QuitMode) -> Result<(), QuitError> {
        let mut state = self.0.lock().unwrap();
        state.quits.push(mode);
        if state.running && mode == QuitMode::Graceful && state.ignores_polite_quit {
            return Err(QuitError::StillRunning);
        }
        state.running = state.restarts_itself;
        Ok(())
    }
    fn launch(&self) -> anyhow::Result<()> {
        let mut state = self.0.lock().unwrap();
        if state.launch_fails {
            bail!("Không tự mở được Claude Desktop. Hãy mở Claude thủ công.");
        }
        state.launches += 1;
        state.running = true;
        Ok(())
    }
    fn flushes_on_quit(&self) -> bool {
        self.0.lock().unwrap().flushes_on_quit
    }
}

/// A sandbox with a fake Claude data directory and an empty account store.
struct Sandbox {
    _tmp: tempfile::TempDir,
    data: PathBuf,
    store_root: PathBuf,
    claude: MockClaude,
    switcher: Switcher,
}

fn sandbox() -> Sandbox {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("Claude");
    let store_root = tmp.path().join("AppSwitchClaude");
    fs::create_dir_all(&data).unwrap();
    // State shared by all accounts, present from the start.
    fs::write(
        data.join("claude_desktop_config.json"),
        "{\"mcpServers\":{}}",
    )
    .unwrap();
    fs::create_dir_all(data.join("Cache")).unwrap();
    fs::write(data.join("Cache").join("data_0"), "shared cache").unwrap();
    fs::write(
        data.join("config.json"),
        "{\n\t\"locale\": \"vi-VN\",\n\t\"scale\": 0\n}",
    )
    .unwrap();

    let claude = MockClaude::new();
    let switcher = Switcher::new(
        Store::open(store_root.clone()).unwrap(),
        Some(data.clone()),
        Box::new(claude.clone()),
    );
    Sandbox {
        _tmp: tmp,
        data,
        store_root,
        claude,
        switcher,
    }
}

fn quiet(_: &str) {}

impl Sandbox {
    /// Make the data directory look as if `who` had just logged in inside Claude.
    fn log_in(&self, who: &str, uuid: Option<&str>, cookies: &[u8]) {
        fs::write(self.data.join("Cookies"), cookies).unwrap();
        fs::write(self.data.join("Cookies-journal"), b"").unwrap();
        self.set_marker(who);
        let idb = self
            .data
            .join("IndexedDB")
            .join("https_claude.ai_0.indexeddb.leveldb");
        fs::create_dir_all(&idb).unwrap();
        fs::write(idb.join("LOG"), format!("idb of {who}")).unwrap();

        let mut config = config_json::read_effective(&self.data)
            .unwrap()
            .unwrap_or_default();
        config.insert("oauth:tokenCache".into(), json!(format!("token of {who}")));
        match uuid {
            Some(uuid) => config.insert("lastKnownAccountUuid".into(), json!(uuid)),
            None => config.shift_remove("lastKnownAccountUuid"),
        };
        fs::write(
            self.data.join("config.json"),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
        let _ = fs::remove_file(self.data.join("config.json.journal"));
    }

    fn set_marker(&self, text: &str) {
        let dir = self.data.join("Local Storage").join("leveldb");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("000003.log"), text).unwrap();
    }

    fn marker(&self) -> Option<String> {
        fs::read_to_string(
            self.data
                .join("Local Storage")
                .join("leveldb")
                .join("000003.log"),
        )
        .ok()
    }

    fn config(&self) -> Value {
        Value::Object(
            config_json::read_effective(&self.data)
                .unwrap()
                .unwrap_or_default(),
        )
    }

    fn live(&self) -> LiveSession {
        self.switcher.overview().unwrap().live
    }

    fn labels(&self) -> Vec<String> {
        let overview = self.switcher.overview().unwrap();
        overview
            .accounts
            .iter()
            .map(|a| a.account.label.clone())
            .collect()
    }

    fn id_of(&self, label: &str) -> String {
        let overview = self.switcher.overview().unwrap();
        let found = overview.accounts.iter().find(|a| a.account.label == label);
        found
            .unwrap_or_else(|| panic!("no account named {label}"))
            .account
            .id
            .clone()
    }

    fn save(&self, label: &str) -> Result<String, OpError> {
        self.switcher
            .save_current(label, SaveOptions::default(), &quiet)
    }

    fn switch(&self, label: &str) -> Result<String, OpError> {
        self.switcher.switch_to(&self.id_of(label), false, &quiet)
    }

    fn snapshot_file(&self, label: &str, rel: &str) -> PathBuf {
        self.store_root
            .join("profiles")
            .join(self.id_of(label))
            .join("files")
            .join(rel)
    }

    /// Shared, account-independent state must survive every operation.
    fn assert_shared_state_untouched(&self) {
        assert_eq!(
            fs::read_to_string(self.data.join("claude_desktop_config.json")).unwrap(),
            "{\"mcpServers\":{}}"
        );
        assert_eq!(
            fs::read_to_string(self.data.join("Cache").join("data_0")).unwrap(),
            "shared cache"
        );
        assert_eq!(self.config()["locale"], "vi-VN");
        assert_eq!(self.config()["scale"], 0);
    }
}

fn saved(sandbox: &Sandbox, label: &str) -> LiveSession {
    LiveSession::Saved {
        id: sandbox.id_of(label),
    }
}

#[test]
fn saves_the_account_claude_is_signed_in_to() {
    let s = sandbox();
    s.log_in("A", Some(UUID_A), COOKIES_A);
    assert_eq!(
        s.live(),
        LiveSession::Unsaved {
            account_uuid: Some(UUID_A.into())
        }
    );

    let message = s.save("  Công   việc ").unwrap();
    assert_eq!(message, "Đã lưu tài khoản “Công việc”.");
    assert_eq!(s.labels(), ["Công việc"]);
    assert_eq!(s.live(), saved(&s, "Công việc"));

    let overview = s.switcher.overview().unwrap();
    let account = &overview.accounts[0];
    assert!(account.is_live && account.has_snapshot);
    assert_eq!(account.account.account_uuid.as_deref(), Some(UUID_A));
    assert_eq!(
        account.session,
        SessionProbe::LoggedIn {
            expires_unix: Some(4_102_444_800)
        }
    );

    assert_eq!(
        fs::read(s.snapshot_file("Công việc", "Cookies")).unwrap(),
        COOKIES_A
    );
    // Claude was not running, so it is neither quit-waited nor started.
    assert_eq!(s.claude.launches(), 0);
    // The live session is only copied, never changed.
    assert_eq!(fs::read(s.data.join("Cookies")).unwrap(), COOKIES_A);
    assert_eq!(s.config()["oauth:tokenCache"], "token of A");
    s.assert_shared_state_untouched();
}

#[test]
fn saving_restarts_claude_only_if_it_was_running() {
    let s = sandbox();
    s.log_in("A", Some(UUID_A), COOKIES_A);
    s.claude.set(|c| c.running = true);
    s.save("A").unwrap();
    assert_eq!(s.claude.quits(), [QuitMode::Graceful]);
    assert_eq!(s.claude.launches(), 1);
    assert!(s.claude.running());
}

#[test]
fn full_cycle_add_a_second_account_and_switch_both_ways() {
    let s = sandbox();
    s.log_in("A", Some(UUID_A), COOKIES_A);
    s.claude.set(|c| c.running = true);
    s.save("Công việc").unwrap();

    // "Add another account": Claude restarts signed out, A stays saved.
    let message = s.switcher.prepare_new_login(false, &quiet).unwrap();
    assert!(
        message.starts_with("Claude đang mở ở màn hình đăng nhập."),
        "{message}"
    );
    assert!(!s.data.join("Cookies").exists());
    assert!(!s.data.join("Local Storage").exists());
    assert!(!s.data.join("IndexedDB").exists());
    assert!(s.config().get("oauth:tokenCache").is_none());
    assert!(s.config().get("lastKnownAccountUuid").is_none());
    assert_eq!(s.live(), LiveSession::LoggedOut);
    assert!(s.claude.running());
    s.assert_shared_state_untouched();

    // The user logs in as B inside Claude, then saves it.
    s.log_in("B", Some(UUID_B), COOKIES_B);
    assert_eq!(
        s.live(),
        LiveSession::Unsaved {
            account_uuid: Some(UUID_B.into())
        }
    );
    s.save("Cá nhân").unwrap();
    assert_eq!(s.labels(), ["Công việc", "Cá nhân"]);
    assert_eq!(s.live(), saved(&s, "Cá nhân"));

    // While B is in use its session moves on (rotated token, new local state).
    s.set_marker("B, later");
    let mut config = config_json::read_effective(&s.data).unwrap().unwrap();
    config.insert("oauth:tokenCache".into(), json!("token of B, rotated"));
    fs::write(
        s.data.join("config.json"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();

    // Switch to A: A's session is back exactly as saved.
    let launches = s.claude.launches();
    assert_eq!(
        s.switch("Công việc").unwrap(),
        "Đã chuyển sang “Công việc”."
    );
    assert_eq!(fs::read(s.data.join("Cookies")).unwrap(), COOKIES_A);
    assert_eq!(s.marker().as_deref(), Some("A"));
    assert_eq!(s.config()["oauth:tokenCache"], "token of A");
    assert_eq!(s.config()["lastKnownAccountUuid"], UUID_A);
    assert_eq!(s.live(), saved(&s, "Công việc"));
    assert_eq!(s.claude.launches(), launches + 1);
    assert!(s.claude.running());
    s.assert_shared_state_untouched();

    // …and B's latest state was captured on the way out, so switching back restores it.
    assert_eq!(s.switch("Cá nhân").unwrap(), "Đã chuyển sang “Cá nhân”.");
    assert_eq!(fs::read(s.data.join("Cookies")).unwrap(), COOKIES_B);
    assert_eq!(s.marker().as_deref(), Some("B, later"));
    assert_eq!(s.config()["oauth:tokenCache"], "token of B, rotated");
    assert_eq!(s.config()["lastKnownAccountUuid"], UUID_B);
    assert_eq!(s.live(), saved(&s, "Cá nhân"));
    assert_eq!(s.labels(), ["Công việc", "Cá nhân"]);
    s.assert_shared_state_untouched();
}

#[test]
fn switching_starts_claude_even_if_it_was_closed() {
    let s = sandbox();
    s.log_in("A", Some(UUID_A), COOKIES_A);
    s.save("A").unwrap();
    s.log_in("B", Some(UUID_B), COOKIES_B);
    s.save("B").unwrap();
    assert!(!s.claude.running());

    s.switch("A").unwrap();
    assert!(s.claude.running());
    assert_eq!(s.claude.launches(), 1);
}

#[test]
fn switching_to_the_account_already_in_use_changes_nothing() {
    let s = sandbox();
    s.log_in("A", Some(UUID_A), COOKIES_A);
    s.save("A").unwrap();
    s.set_marker("A, with newer local state");

    assert_eq!(
        s.switch("A").unwrap(),
        "Claude đang dùng tài khoản “A” rồi."
    );
    // The live session is newer than the snapshot and must not be rolled back.
    assert_eq!(s.marker().as_deref(), Some("A, with newer local state"));
    assert!(s.claude.running());
}

#[test]
fn an_unsaved_session_is_kept_when_switching_away() {
    let s = sandbox();
    s.log_in("A", Some(UUID_A), COOKIES_A);
    s.save("A").unwrap();
    // The user signs in to another account inside Claude, without telling this tool.
    s.log_in("C", Some(UUID_C), COOKIES_B);

    let message = s.switch("A").unwrap();
    assert!(message.contains("“Tài khoản chưa đặt tên”"), "{message}");
    assert_eq!(s.labels(), ["A", "Tài khoản chưa đặt tên"]);
    assert_eq!(s.marker().as_deref(), Some("A"));

    // Nothing was lost: the unnamed entry switches back to C.
    s.switch("Tài khoản chưa đặt tên").unwrap();
    assert_eq!(s.marker().as_deref(), Some("C"));
    assert_eq!(s.config()["lastKnownAccountUuid"], UUID_C);
    assert_eq!(s.config()["oauth:tokenCache"], "token of C");
}

#[test]
fn saving_an_account_twice_updates_it_instead_of_duplicating() {
    let s = sandbox();
    s.log_in("A", Some(UUID_A), COOKIES_A);
    s.save("Công việc").unwrap();
    s.set_marker("A, refreshed");

    let message = s.save("Tên khác").unwrap();
    assert!(
        message.contains("đã có trong danh sách với tên “Công việc”"),
        "{message}"
    );
    assert_eq!(s.labels(), ["Công việc"]);
    let snapshot = s.snapshot_file("Công việc", "Local Storage/leveldb/000003.log");
    assert_eq!(fs::read_to_string(snapshot).unwrap(), "A, refreshed");
}

#[test]
fn refuses_to_save_when_nobody_is_logged_in() {
    let s = sandbox();
    s.log_in("nobody", None, COOKIES_LOGGED_OUT);
    s.claude.set(|c| c.running = true);
    assert_eq!(s.live(), LiveSession::LoggedOut);

    assert!(matches!(s.save("X"), Err(OpError::NotLoggedIn)));
    assert!(s.labels().is_empty());
    // Claude was running, so it is brought back.
    assert!(s.claude.running());

    // An expired session cookie is not a login either.
    s.log_in("expired", Some(UUID_A), COOKIES_EXPIRED);
    assert!(matches!(s.save("X"), Err(OpError::NotLoggedIn)));

    // The user can insist.
    let options = SaveOptions {
        allow_unverified: true,
        ..SaveOptions::default()
    };
    s.switcher.save_current("X", options, &quiet).unwrap();
    assert_eq!(s.labels(), ["X"]);
}

#[test]
fn a_blank_or_overlong_name_is_rejected_before_anything_happens() {
    let s = sandbox();
    s.log_in("A", Some(UUID_A), COOKIES_A);
    s.claude.set(|c| c.running = true);
    assert!(matches!(s.save("   "), Err(OpError::Failed(_))));
    assert!(matches!(s.save(&"x".repeat(200)), Err(OpError::Failed(_))));
    assert!(s.claude.quits().is_empty());
    assert!(s.claude.running());
}

#[test]
fn asks_before_force_quitting_a_claude_that_will_not_quit() {
    let s = sandbox();
    s.log_in("A", Some(UUID_A), COOKIES_A);
    s.save("A").unwrap();
    s.log_in("B", Some(UUID_B), COOKIES_B);
    s.save("B").unwrap();
    s.claude.set(|c| {
        c.running = true;
        c.ignores_polite_quit = true;
    });

    assert!(matches!(s.switch("A"), Err(OpError::ClaudeStillRunning)));
    // Nothing was touched.
    assert_eq!(s.marker().as_deref(), Some("B"));
    assert_eq!(s.live(), saved(&s, "B"));
    assert!(matches!(
        s.switcher.prepare_new_login(false, &quiet),
        Err(OpError::ClaudeStillRunning)
    ));
    assert!(matches!(s.save("B"), Err(OpError::ClaudeStillRunning)));

    // With the user's consent the processes are ended and the switch goes through.
    s.switcher.switch_to(&s.id_of("A"), true, &quiet).unwrap();
    assert_eq!(s.claude.quits().last(), Some(&QuitMode::Force));
    assert_eq!(s.marker().as_deref(), Some("A"));
}

#[test]
fn removing_an_account_forgets_it_but_leaves_claude_alone() {
    let s = sandbox();
    s.log_in("A", Some(UUID_A), COOKIES_A);
    s.save("A").unwrap();
    let id = s.id_of("A");
    let profile = s.store_root.join("profiles").join(&id);
    assert!(profile.is_dir());

    assert_eq!(
        s.switcher.remove(&id).unwrap(),
        "Đã xoá “A” khỏi danh sách."
    );
    assert!(s.labels().is_empty());
    assert!(!profile.exists());
    // Claude stays signed in; the session simply is not in the list any more.
    assert_eq!(fs::read(s.data.join("Cookies")).unwrap(), COOKIES_A);
    assert_eq!(
        s.live(),
        LiveSession::Unsaved {
            account_uuid: Some(UUID_A.into())
        }
    );

    assert!(matches!(s.switcher.remove(&id), Err(OpError::Failed(_))));
    assert!(matches!(
        s.switcher.switch_to(&id, false, &quiet),
        Err(OpError::Failed(_))
    ));
}

#[test]
fn renames_an_account() {
    let s = sandbox();
    s.log_in("A", Some(UUID_A), COOKIES_A);
    s.save("A").unwrap();
    let id = s.id_of("A");
    assert_eq!(
        s.switcher.rename(&id, " Công ty ").unwrap(),
        "Đã đổi tên thành “Công ty”."
    );
    assert_eq!(s.labels(), ["Công ty"]);
    assert!(matches!(
        s.switcher.rename(&id, ""),
        Err(OpError::Failed(_))
    ));
    assert!(matches!(
        s.switcher.rename("acc-missing", "X"),
        Err(OpError::Failed(_))
    ));
    assert_eq!(s.labels(), ["Công ty"]);
}

#[test]
fn reads_the_newest_tokens_from_claudes_config_journal() {
    let s = sandbox();
    s.log_in("A", Some(UUID_A), COOKIES_A);
    // Claude quit leaving a pending journal with a rotated token, as it does in practice.
    let base = fs::read(s.data.join("config.json")).unwrap();
    let mut newer = config_json::read_effective(&s.data).unwrap().unwrap();
    newer.insert("oauth:tokenCache".into(), json!("token of A, rotated"));
    let body = serde_json::to_vec(&newer).unwrap();
    let journal_path = s.data.join("config.json.journal");
    fs::write(&journal_path, journal(&base, &body)).unwrap();

    s.save("A").unwrap();
    s.log_in("B", Some(UUID_B), COOKIES_B);
    s.save("B").unwrap();
    s.switch("A").unwrap();

    assert_eq!(s.config()["oauth:tokenCache"], "token of A, rotated");
    // No stale journal is left to override the restored config on Claude's next start.
    assert!(!journal_path.exists());
    let on_disk: Value =
        serde_json::from_slice(&fs::read(s.data.join("config.json")).unwrap()).unwrap();
    assert_eq!(on_disk["oauth:tokenCache"], "token of A, rotated");
    assert_eq!(on_disk["locale"], "vi-VN");
}

fn journal(base: &[u8], body: &[u8]) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    let hex = |bytes: &[u8]| -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    };
    let mut out = format!("{} {} .\n", hex(body), hex(base)).into_bytes();
    out.extend_from_slice(body);
    out
}

#[test]
fn an_account_saved_before_claude_recorded_its_id_is_still_recognised() {
    let s = sandbox();
    s.log_in("A", None, COOKIES_A);
    assert_eq!(s.live(), LiveSession::Unsaved { account_uuid: None });
    s.save("A").unwrap();
    assert_eq!(s.live(), saved(&s, "A"));

    // Claude writes the id a moment later.
    s.log_in("A", Some(UUID_A), COOKIES_A);
    assert_eq!(s.live(), saved(&s, "A"));

    // The next snapshot learns the id; no duplicate entry appears.
    s.switcher.prepare_new_login(false, &quiet).unwrap();
    let overview = s.switcher.overview().unwrap();
    assert_eq!(overview.accounts.len(), 1);
    assert_eq!(
        overview.accounts[0].account.account_uuid.as_deref(),
        Some(UUID_A)
    );
}

#[test]
fn an_interrupted_switch_never_overwrites_a_saved_account() {
    let s = sandbox();
    s.log_in("A", Some(UUID_A), COOKIES_A);
    s.save("A").unwrap();
    s.log_in("B", Some(UUID_B), COOKIES_B);
    s.save("B").unwrap();

    // Something restarts Claude in the middle of the switch.
    s.claude.set(|c| c.restarts_itself = true);
    let error = s.switch("A").unwrap_err();
    assert!(
        error
            .to_string()
            .contains("mở lại trong lúc đang đổi tài khoản"),
        "{error}"
    );
    assert_eq!(s.live(), LiveSession::Interrupted);

    // The live session is now a mix nobody should keep. Simulate the damage…
    s.set_marker("half A, half B");
    // …saving it is refused,
    assert!(matches!(s.save("mixed"), Err(OpError::Failed(_))));
    assert_eq!(s.labels(), ["A", "B"]);

    // …and the retry replaces it without copying it over anyone's snapshot.
    s.claude.set(|c| c.restarts_itself = false);
    s.switch("B").unwrap();
    assert_eq!(s.marker().as_deref(), Some("B"));
    assert_eq!(s.live(), saved(&s, "B"));
    assert_eq!(
        fs::read_to_string(s.snapshot_file("A", "Local Storage/leveldb/000003.log")).unwrap(),
        "A"
    );
    assert_eq!(s.labels(), ["A", "B"]);
}

#[test]
fn a_failed_launch_is_reported_without_undoing_the_switch() {
    let s = sandbox();
    s.log_in("A", Some(UUID_A), COOKIES_A);
    s.save("A").unwrap();
    s.log_in("B", Some(UUID_B), COOKIES_B);
    s.save("B").unwrap();
    s.claude.set(|c| c.launch_fails = true);

    let message = s.switch("A").unwrap();
    assert_eq!(
        message,
        "Đã chuyển sang “A”. Không tự mở được Claude Desktop. Hãy mở Claude thủ công."
    );
    assert_eq!(s.live(), saved(&s, "A"));
}

#[test]
fn a_fresh_login_still_in_claudes_memory_shows_up_but_is_not_captured_blindly() {
    let s = sandbox();
    s.log_in("A", Some(UUID_A), COOKIES_A);
    s.save("A").unwrap();
    s.switcher.prepare_new_login(false, &quiet).unwrap();

    // Claude is running and has recorded the new account, but the login cookie has not
    // reached the disk yet.
    s.log_in("B", Some(UUID_B), COOKIES_LOGGED_OUT);
    assert!(s.claude.running());
    assert_eq!(
        s.live(),
        LiveSession::Unsaved {
            account_uuid: Some(UUID_B.into())
        }
    );

    // Operations decide with Claude stopped: this state has no login to save.
    assert!(matches!(s.save("B"), Err(OpError::NotLoggedIn)));
    assert_eq!(s.labels(), ["A"]);
}

#[test]
fn does_not_wait_for_a_flush_when_the_login_is_already_on_disk() {
    let s = sandbox();
    s.log_in("A", Some(UUID_A), COOKIES_A);
    // A platform where quitting does not flush (Windows), with Claude running.
    s.claude.set(|c| {
        c.flushes_on_quit = false;
        c.running = true;
    });
    let started = std::time::Instant::now();
    s.save("A").unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
}

#[test]
fn reports_when_claude_is_not_installed() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("no-such-dir");
    for data_dir in [None, Some(missing)] {
        let switcher = Switcher::new(
            Store::open(tmp.path().join("store")).unwrap(),
            data_dir,
            Box::new(MockClaude::new()),
        );
        let overview = switcher.overview().unwrap();
        assert_eq!(overview.live, LiveSession::NoDataDir);
        assert!(matches!(
            switcher.save_current("A", SaveOptions::default(), &quiet),
            Err(OpError::Failed(_))
        ));
        assert!(matches!(
            switcher.prepare_new_login(false, &quiet),
            Err(OpError::Failed(_))
        ));
    }
}

#[test]
fn shows_when_a_saved_session_has_expired() {
    let s = sandbox();
    s.log_in("A", Some(UUID_A), COOKIES_A);
    s.save("A").unwrap();
    s.log_in("B", Some(UUID_B), COOKIES_B);
    s.save("B").unwrap();
    // A's saved cookie store is replaced by one whose session ran out.
    fs::write(s.snapshot_file("A", "Cookies"), COOKIES_EXPIRED).unwrap();

    let overview = s.switcher.overview().unwrap();
    let a = overview
        .accounts
        .iter()
        .find(|v| v.account.label == "A")
        .unwrap();
    assert!(!a.is_live);
    assert!(a.session.is_expired(app_switch_claude::fsutil::now_unix()));
    let b = overview
        .accounts
        .iter()
        .find(|v| v.account.label == "B")
        .unwrap();
    assert!(b.is_live);
    assert_eq!(
        b.session,
        SessionProbe::LoggedIn {
            expires_unix: Some(4_133_980_800)
        }
    );
}

#[test]
fn works_with_the_windows_cookie_location() {
    let s = sandbox();
    let network = s.data.join("Network");
    fs::create_dir_all(&network).unwrap();
    fs::write(network.join("Network Persistent State"), "shared").unwrap();
    fs::write(
        s.data.join("Local State"),
        "{\"os_crypt\":{\"encrypted_key\":\"k\"}}",
    )
    .unwrap();

    let log_in = |who: &str, uuid: &str, cookies: &[u8]| {
        s.log_in(who, Some(uuid), cookies);
        fs::rename(s.data.join("Cookies"), network.join("Cookies")).unwrap();
        fs::remove_file(s.data.join("Cookies-journal")).unwrap();
    };
    log_in("A", UUID_A, COOKIES_A);
    s.save("A").unwrap();
    log_in("B", UUID_B, COOKIES_B);
    s.save("B").unwrap();

    s.switch("A").unwrap();
    assert_eq!(fs::read(network.join("Cookies")).unwrap(), COOKIES_A);
    assert!(!s.data.join("Cookies").exists());
    assert_eq!(
        fs::read_to_string(network.join("Network Persistent State")).unwrap(),
        "shared"
    );
    // The key that decrypts every saved cookie store is never touched.
    assert_eq!(
        fs::read_to_string(s.data.join("Local State")).unwrap(),
        "{\"os_crypt\":{\"encrypted_key\":\"k\"}}"
    );
    assert_eq!(s.live(), saved(&s, "A"));
}

#[test]
fn the_store_is_private_to_the_user() {
    let s = sandbox();
    s.log_in("A", Some(UUID_A), COOKIES_A);
    s.save("A").unwrap();
    assert_private(&s.store_root);
    assert_private(&s.store_root.join("profiles"));
}

#[cfg(unix)]
fn assert_private(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700, "{} should be owner-only", path.display());
}

#[cfg(not(unix))]
fn assert_private(path: &Path) {
    assert!(path.is_dir());
}
