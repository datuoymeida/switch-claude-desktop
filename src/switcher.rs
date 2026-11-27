//! The operations behind the UI: save the live session, switch, add, rename, remove.
//!
//! Vocabulary: the *live session* is whatever account Claude Desktop's data directory is
//! signed in to right now. A *saved account* is a snapshot of a live session kept in this
//! tool's store. Switching replaces the live session with a snapshot while Claude is not
//! running, after first refreshing the snapshot of the session being replaced (tokens and
//! cookies rotate, so the newest copy is the only one sure to work).

use anyhow::{Context, Result, anyhow, bail};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::claude_app::{ClaudeControl, DesktopClaude, QuitError, QuitMode};
use crate::config_json::{self, ConfigMap};
use crate::cookies::{self, SessionProbe};
use crate::fsutil::now_unix;
use crate::paths;
use crate::session;
use crate::store::{Account, Registry, Store, StoreLock, normalize_label};

/// Chromium writes cookie changes to disk at most this long after they happen.
const LOGIN_FLUSH_WAIT_SECS: u32 = 35;

const UNNAMED_LABEL: &str = "Tài khoản chưa đặt tên";
const INTERRUPTED_MESSAGE: &str = "Lần đổi tài khoản trước chưa hoàn tất nên phiên trong Claude \
     không đáng tin. Hãy chọn một tài khoản đã lưu và bấm Chuyển.";
const REOPENED_MESSAGE: &str =
    "Claude đã được mở lại trong lúc đang đổi tài khoản. Hãy bấm Chuyển lần nữa.";
const NOT_IN_LIST_MESSAGE: &str = "Tài khoản này không còn trong danh sách.";

/// Receives short status lines while an operation runs.
pub type Progress<'a> = &'a dyn Fn(&str);

#[derive(Debug)]
pub enum OpError {
    /// Claude did not quit when asked. Retrying with `force_quit` ends its processes.
    ClaudeStillRunning,
    /// No login session was found in Claude. Retrying with `allow_unverified` saves the
    /// current state anyway.
    NotLoggedIn,
    Failed(anyhow::Error),
}

impl fmt::Display for OpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OpError::ClaudeStillRunning => f.write_str("Claude chưa thoát."),
            OpError::NotLoggedIn => f.write_str("Claude chưa đăng nhập tài khoản nào."),
            OpError::Failed(error) => write!(f, "{error:#}"),
        }
    }
}

impl From<anyhow::Error> for OpError {
    fn from(error: anyhow::Error) -> Self {
        OpError::Failed(error)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SaveOptions {
    /// End Claude's processes instead of asking it to quit.
    pub force_quit: bool,
    /// Save even though no login cookie was detected.
    pub allow_unverified: bool,
}

/// Which account the live session belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveSession {
    /// Claude Desktop's data directory was not found.
    NoDataDir,
    /// A previous switch stopped half-way; the live session is a mix and must be replaced.
    Interrupted,
    LoggedOut,
    /// Signed in to the saved account with this id.
    Saved {
        id: String,
    },
    /// Signed in to an account that is not in the list yet.
    Unsaved {
        account_uuid: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountView {
    pub account: Account,
    /// This account is the one Claude is signed in to right now.
    pub is_live: bool,
    pub has_snapshot: bool,
    /// Login state of the saved session (of the live session for the live account).
    pub session: SessionProbe,
}

/// Everything the UI shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overview {
    pub data_dir: Option<PathBuf>,
    pub store_dir: PathBuf,
    pub claude_running: bool,
    pub live: LiveSession,
    pub accounts: Vec<AccountView>,
}

/// What Claude's data directory says about the signed-in account.
struct Live {
    probe: SessionProbe,
    /// The account id Claude recorded in its config, if any.
    uuid: Option<String>,
}

fn read_live(data_dir: &Path) -> Live {
    let uuid = config_json::read_effective(data_dir)
        .ok()
        .flatten()
        .as_ref()
        .and_then(config_json::account_uuid);
    Live {
        probe: cookies::probe_session(data_dir),
        uuid,
    }
}

/// Decide which account the live session belongs to.
///
/// `claude_is_running` relaxes the login check: a running Claude holds a fresh login in
/// memory for up to half a minute before the cookie reaches the disk, while its config
/// already names the account. Operations always decide with Claude stopped.
fn resolve(registry: &Registry, live: &Live, now: i64, claude_is_running: bool) -> LiveSession {
    if registry.rewrite_in_progress {
        return LiveSession::Interrupted;
    }
    let logged_in = match live.probe {
        SessionProbe::LoggedIn { .. } => live.probe.is_active(now),
        SessionProbe::LoggedOut => claude_is_running && live.uuid.is_some(),
        // The cookie store could not be read: trust what Claude itself recorded.
        SessionProbe::Unknown => live.uuid.is_some(),
    };
    if !logged_in {
        return LiveSession::LoggedOut;
    }
    if let Some(uuid) = &live.uuid
        && let Some(account) = registry.find_by_uuid(uuid)
    {
        return LiveSession::Saved {
            id: account.id.clone(),
        };
    }
    // A snapshot taken before Claude had recorded the account id can only be recognised
    // through our own bookkeeping.
    if let Some(active) = registry.active()
        && active.account_uuid.is_none()
    {
        return LiveSession::Saved {
            id: active.id.clone(),
        };
    }
    LiveSession::Unsaved {
        account_uuid: live.uuid.clone(),
    }
}

pub struct Switcher {
    store: Store,
    data_dir: Option<PathBuf>,
    claude: Box<dyn ClaudeControl>,
    /// Keeps other copies of the app away from the store while this one runs.
    _lock: Option<StoreLock>,
}

impl Switcher {
    pub fn new(store: Store, data_dir: Option<PathBuf>, claude: Box<dyn ClaudeControl>) -> Self {
        Self {
            store,
            data_dir,
            claude,
            _lock: None,
        }
    }

    /// The switcher for the Claude Desktop installation on this machine.
    pub fn for_this_machine() -> Result<Self> {
        let data_dir = paths::claude_data_dir();
        let store = Store::open(paths::store_dir()?)?;
        let lock = store.lock()?;
        let claude = DesktopClaude::new(data_dir.clone())?;
        let mut switcher = Self::new(store, data_dir, Box::new(claude));
        switcher._lock = Some(lock);
        Ok(switcher)
    }

    fn data_dir(&self) -> Result<&Path> {
        match self.data_dir.as_deref() {
            Some(dir) if dir.is_dir() => Ok(dir),
            Some(dir) => bail!(
                "Không tìm thấy dữ liệu của Claude Desktop tại {}. Hãy mở Claude ít nhất một lần.",
                dir.display()
            ),
            None => bail!(
                "Không tìm thấy Claude Desktop trên máy này. Hãy cài và mở Claude ít nhất một lần."
            ),
        }
    }

    /// The current state of Claude and of the saved accounts.
    pub fn overview(&self) -> Result<Overview> {
        let registry = self.store.load()?;
        let now = now_unix();
        let claude_running = self.claude.is_running();

        let (live, live_probe) = match self.data_dir.as_deref().filter(|dir| dir.is_dir()) {
            Some(dir) => {
                let live = read_live(dir);
                (resolve(&registry, &live, now, claude_running), live.probe)
            }
            None => (LiveSession::NoDataDir, SessionProbe::LoggedOut),
        };
        let live_id = match &live {
            LiveSession::Saved { id } => Some(id.as_str()),
            _ => None,
        };

        let accounts = registry
            .accounts
            .iter()
            .map(|account| {
                let is_live = live_id == Some(account.id.as_str());
                let has_snapshot = self.store.has_snapshot(&account.id);
                let session = match self.store.files_dir(&account.id) {
                    // The live cookie is newer than the copy taken at the last switch.
                    _ if is_live && live_probe != SessionProbe::LoggedOut => live_probe,
                    Ok(files) if has_snapshot => cookies::probe_session(&files),
                    _ => SessionProbe::LoggedOut,
                };
                AccountView {
                    account: account.clone(),
                    is_live,
                    has_snapshot,
                    session,
                }
            })
            .collect();

        Ok(Overview {
            data_dir: self.data_dir.clone(),
            store_dir: self.store.root().to_path_buf(),
            claude_running,
            live,
            accounts,
        })
    }

    /// Save the account Claude is signed in to under `label`. If that account is already
    /// in the list, its saved session is refreshed instead and its name is kept.
    pub fn save_current(
        &self,
        label: &str,
        options: SaveOptions,
        progress: Progress<'_>,
    ) -> Result<String, OpError> {
        let dir = self.data_dir()?;
        let label = normalize_label(label)?;
        if self.store.load()?.rewrite_in_progress {
            return Err(anyhow!(INTERRUPTED_MESSAGE).into());
        }

        self.wait_for_login_on_disk(dir, progress);
        let was_running = self.stop_claude(options.force_quit, progress)?;
        let live = read_live(dir);
        let now = now_unix();
        if !live.probe.is_active(now) && !options.allow_unverified {
            if was_running {
                progress("Đang mở lại Claude…");
                let _ = self.claude.launch();
            }
            return Err(OpError::NotLoggedIn);
        }

        progress("Đang lưu phiên đăng nhập…");
        let mut registry = self.store.load()?;
        let existing = live
            .uuid
            .as_deref()
            .and_then(|uuid| registry.find_by_uuid(uuid))
            .map(|account| account.id.clone())
            .or_else(|| match resolve(&registry, &live, now, false) {
                LiveSession::Saved { id } => Some(id),
                _ => None,
            });

        let message = match existing {
            Some(id) => {
                self.capture_profile(dir, &id)?;
                let account = registry.find_mut(&id).context(NOT_IN_LIST_MESSAGE)?;
                account.saved_at = now;
                account.last_used_at = Some(now);
                if live.uuid.is_some() {
                    account.account_uuid = live.uuid.clone();
                }
                let name = account.label.clone();
                registry.active_id = Some(id);
                format!(
                    "Tài khoản này đã có trong danh sách với tên “{name}” — đã cập nhật phiên đăng nhập."
                )
            }
            None => {
                let id = self.store.new_id();
                self.capture_profile(dir, &id).inspect_err(|_| {
                    let _ = self.store.remove_profile(&id);
                })?;
                registry.accounts.push(Account {
                    id: id.clone(),
                    label: label.clone(),
                    account_uuid: live.uuid.clone(),
                    created_at: now,
                    saved_at: now,
                    last_used_at: Some(now),
                });
                registry.active_id = Some(id);
                format!("Đã lưu tài khoản “{label}”.")
            }
        };
        self.store.save(&registry)?;

        Ok(if was_running {
            self.launch_and_report(message, progress)
        } else {
            message
        })
    }

    /// Put the saved account `id` into Claude and start Claude.
    pub fn switch_to(
        &self,
        id: &str,
        force_quit: bool,
        progress: Progress<'_>,
    ) -> Result<String, OpError> {
        let dir = self.data_dir()?;
        let target = self
            .store
            .load()?
            .find(id)
            .cloned()
            .context(NOT_IN_LIST_MESSAGE)?;
        if !self.store.has_snapshot(id) {
            return Err(anyhow!(
                "Tài khoản “{}” chưa có phiên đăng nhập được lưu.",
                target.label
            )
            .into());
        }

        self.wait_for_login_on_disk(dir, progress);
        self.stop_claude(force_quit, progress)?;

        let mut registry = self.store.load()?;
        let live = read_live(dir);
        let now = now_unix();
        if resolve(&registry, &live, now, false) == (LiveSession::Saved { id: id.to_owned() }) {
            // Claude is already on this account; its live session is the newest copy.
            registry.active_id = Some(id.to_owned());
            if let Some(account) = registry.find_mut(id) {
                account.last_used_at = Some(now);
            }
            self.store.save(&registry)?;
            let message = format!("Claude đang dùng tài khoản “{}” rồi.", target.label);
            return Ok(self.launch_and_report(message, progress));
        }

        let note = self.preserve_live(dir, &mut registry, &live, now, progress)?;
        // From here on the live session is in flux; record that before touching it.
        registry.rewrite_in_progress = true;
        self.store.save(&registry)?;

        progress(&format!("Đang nạp phiên của “{}”…", target.label));
        session::restore(&self.store.files_dir(id)?, dir)?;
        config_json::replace_account_keys(dir, &self.store.read_keys(id)?)?;
        if self.claude.is_running() {
            // Claude may have read a half-replaced session. Leave the marker set so the
            // next attempt replaces the session without saving this one.
            return Err(anyhow!(REOPENED_MESSAGE).into());
        }

        registry.rewrite_in_progress = false;
        registry.active_id = Some(id.to_owned());
        if let Some(account) = registry.find_mut(id) {
            account.last_used_at = Some(now);
        }
        self.store.save(&registry)?;

        let mut message = format!("Đã chuyển sang “{}”.", target.label);
        if let Some(note) = note {
            message.push(' ');
            message.push_str(&note);
        }
        Ok(self.launch_and_report(message, progress))
    }

    /// Save the live session, then start Claude signed out so another account can log in.
    /// Nothing is logged out on the server, so the saved sessions keep working.
    pub fn prepare_new_login(
        &self,
        force_quit: bool,
        progress: Progress<'_>,
    ) -> Result<String, OpError> {
        let dir = self.data_dir()?;
        self.wait_for_login_on_disk(dir, progress);
        self.stop_claude(force_quit, progress)?;

        let mut registry = self.store.load()?;
        let live = read_live(dir);
        let now = now_unix();
        let note = self.preserve_live(dir, &mut registry, &live, now, progress)?;
        registry.rewrite_in_progress = true;
        self.store.save(&registry)?;

        progress("Đang đưa Claude về màn hình đăng nhập…");
        session::clear(dir)?;
        config_json::replace_account_keys(dir, &ConfigMap::new())?;
        if self.claude.is_running() {
            return Err(anyhow!(REOPENED_MESSAGE).into());
        }

        registry.rewrite_in_progress = false;
        registry.active_id = None;
        self.store.save(&registry)?;

        let mut message = String::from(
            "Claude đang mở ở màn hình đăng nhập. Hãy đăng nhập tài khoản mới, rồi quay lại đây để lưu.",
        );
        if let Some(note) = note {
            message.push(' ');
            message.push_str(&note);
        }
        Ok(self.launch_and_report(message, progress))
    }

    /// Forget a saved account. The Claude account itself and the live session are untouched.
    pub fn remove(&self, id: &str) -> Result<String, OpError> {
        let mut registry = self.store.load()?;
        let index = registry
            .accounts
            .iter()
            .position(|account| account.id == id)
            .context(NOT_IN_LIST_MESSAGE)?;
        let account = registry.accounts.remove(index);
        if registry.active_id.as_deref() == Some(id) {
            registry.active_id = None;
        }
        self.store.save(&registry)?;
        // The entry is gone from the list; a leftover folder is harmless.
        let _ = self.store.remove_profile(id);
        Ok(format!("Đã xoá “{}” khỏi danh sách.", account.label))
    }

    pub fn rename(&self, id: &str, label: &str) -> Result<String, OpError> {
        let label = normalize_label(label)?;
        let mut registry = self.store.load()?;
        registry.find_mut(id).context(NOT_IN_LIST_MESSAGE)?.label = label.clone();
        self.store.save(&registry)?;
        Ok(format!("Đã đổi tên thành “{label}”."))
    }

    pub fn open_claude(&self) -> Result<String, OpError> {
        self.claude.launch()?;
        Ok(String::from("Đã mở Claude."))
    }

    /// Quit Claude if it is running. Returns whether it was.
    fn stop_claude(&self, force: bool, progress: Progress<'_>) -> Result<bool, OpError> {
        let was_running = self.claude.is_running();
        if was_running {
            progress("Đang thoát Claude…");
        }
        // Called even when the app is not running: helper processes may still be exiting.
        let mode = if force {
            QuitMode::Force
        } else {
            QuitMode::Graceful
        };
        match self.claude.quit(mode) {
            Ok(()) => Ok(was_running),
            Err(QuitError::StillRunning) => Err(OpError::ClaudeStillRunning),
            Err(QuitError::Failed(error)) => Err(OpError::Failed(error)),
        }
    }

    /// Where quitting does not flush Claude's cookie store, a login from the last half
    /// minute may not be on disk yet. Wait for it rather than capture a session that lacks
    /// its login cookie.
    fn wait_for_login_on_disk(&self, dir: &Path, progress: Progress<'_>) {
        if self.claude.flushes_on_quit() || !self.claude.is_running() {
            return;
        }
        // Claude records the account id as soon as a login completes.
        if read_live(dir).uuid.is_none() {
            return;
        }
        for second in 0..LOGIN_FLUSH_WAIT_SECS {
            if cookies::probe_session(dir).is_active(now_unix()) {
                return;
            }
            if second == 0 {
                progress("Đang chờ Claude ghi phiên đăng nhập xuống đĩa…");
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    /// Copy the live session (files and config entries) into the store as profile `id`.
    fn capture_profile(&self, dir: &Path, id: &str) -> Result<()> {
        // Read the config first: if it is unreadable, fail before replacing the old copy.
        let keys = config_json::read_effective(dir)?
            .map(|config| config_json::account_keys(&config))
            .unwrap_or_default();
        session::capture(dir, &self.store.files_dir(id)?)?;
        self.store.write_keys(id, &keys)
    }

    /// Save the session that is about to be replaced. Returns a note for the user when it
    /// had to be added to the list as a new entry.
    fn preserve_live(
        &self,
        dir: &Path,
        registry: &mut Registry,
        live: &Live,
        now: i64,
        progress: Progress<'_>,
    ) -> Result<Option<String>> {
        match resolve(registry, live, now, false) {
            LiveSession::Saved { id } => {
                progress("Đang lưu lại phiên hiện tại…");
                self.capture_profile(dir, &id)?;
                if let Some(account) = registry.find_mut(&id) {
                    account.saved_at = now;
                    if live.uuid.is_some() {
                        account.account_uuid = live.uuid.clone();
                    }
                }
                Ok(None)
            }
            LiveSession::Unsaved { account_uuid } => {
                progress("Đang lưu phiên chưa có trong danh sách…");
                let id = self.store.new_id();
                self.capture_profile(dir, &id).inspect_err(|_| {
                    let _ = self.store.remove_profile(&id);
                })?;
                let label = unused_label(registry);
                registry.accounts.push(Account {
                    id,
                    label: label.clone(),
                    account_uuid,
                    created_at: now,
                    saved_at: now,
                    last_used_at: Some(now),
                });
                Ok(Some(format!(
                    "Tài khoản đang đăng nhập trước đó chưa có trong danh sách nên đã được lưu thành “{label}”."
                )))
            }
            LiveSession::LoggedOut | LiveSession::Interrupted | LiveSession::NoDataDir => Ok(None),
        }
    }

    /// Start Claude; a failure to do so becomes part of the message rather than an error,
    /// because the operation itself has already succeeded.
    fn launch_and_report(&self, mut message: String, progress: Progress<'_>) -> String {
        progress("Đang mở Claude…");
        if let Err(error) = self.claude.launch() {
            message.push(' ');
            message.push_str(&format!("{error:#}"));
        }
        message
    }
}

/// "Tài khoản chưa đặt tên", numbered if that name is taken.
fn unused_label(registry: &Registry) -> String {
    let taken = |label: &str| registry.accounts.iter().any(|a| a.label == label);
    if !taken(UNNAMED_LABEL) {
        return UNNAMED_LABEL.to_owned();
    }
    (2..)
        .map(|n| format!("{UNNAMED_LABEL} {n}"))
        .find(|label| !taken(label))
        .expect("an unused number always exists")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry(accounts: &[(&str, Option<&str>)], active: Option<&str>) -> Registry {
        Registry {
            version: 1,
            active_id: active.map(Into::into),
            rewrite_in_progress: false,
            accounts: accounts
                .iter()
                .map(|(id, uuid)| Account {
                    id: (*id).into(),
                    label: (*id).into(),
                    account_uuid: uuid.map(Into::into),
                    created_at: 0,
                    saved_at: 0,
                    last_used_at: None,
                })
                .collect(),
        }
    }

    const NOW: i64 = 1_800_000_000;
    const LOGGED_IN: SessionProbe = SessionProbe::LoggedIn {
        expires_unix: Some(NOW + 1000),
    };
    const EXPIRED: SessionProbe = SessionProbe::LoggedIn {
        expires_unix: Some(NOW - 1),
    };

    fn live(probe: SessionProbe, uuid: Option<&str>) -> Live {
        Live {
            probe,
            uuid: uuid.map(Into::into),
        }
    }

    fn saved(id: &str) -> LiveSession {
        LiveSession::Saved { id: id.into() }
    }

    #[test]
    fn recognises_a_saved_account_by_claudes_account_id() {
        let reg = registry(&[("a", Some("uuid-a")), ("b", Some("uuid-b"))], Some("a"));
        // The id wins even when our own "active" record points elsewhere.
        assert_eq!(
            resolve(&reg, &live(LOGGED_IN, Some("uuid-b")), NOW, false),
            saved("b")
        );
        assert_eq!(
            resolve(&reg, &live(LOGGED_IN, Some("uuid-c")), NOW, false),
            LiveSession::Unsaved {
                account_uuid: Some("uuid-c".into())
            }
        );
        // No id recorded and our active account has one: do not guess.
        assert_eq!(
            resolve(&reg, &live(LOGGED_IN, None), NOW, false),
            LiveSession::Unsaved { account_uuid: None }
        );
    }

    #[test]
    fn adopts_the_active_account_saved_before_claude_recorded_an_id() {
        let reg = registry(&[("a", None), ("b", Some("uuid-b"))], Some("a"));
        assert_eq!(
            resolve(&reg, &live(LOGGED_IN, None), NOW, false),
            saved("a")
        );
        assert_eq!(
            resolve(&reg, &live(LOGGED_IN, Some("uuid-new")), NOW, false),
            saved("a")
        );
        // Another account's id still takes precedence.
        assert_eq!(
            resolve(&reg, &live(LOGGED_IN, Some("uuid-b")), NOW, false),
            saved("b")
        );
    }

    #[test]
    fn logged_out_and_expired_sessions_belong_to_nobody() {
        let reg = registry(&[("a", Some("uuid-a"))], Some("a"));
        let stale_id = live(SessionProbe::LoggedOut, Some("uuid-a"));
        assert_eq!(resolve(&reg, &stale_id, NOW, false), LiveSession::LoggedOut);
        assert_eq!(
            resolve(&reg, &live(EXPIRED, Some("uuid-a")), NOW, false),
            LiveSession::LoggedOut
        );
        assert_eq!(
            resolve(&reg, &live(EXPIRED, Some("uuid-a")), NOW, true),
            LiveSession::LoggedOut
        );
    }

    #[test]
    fn a_running_claude_may_not_have_flushed_its_login_yet() {
        let reg = registry(&[("a", Some("uuid-a"))], None);
        let pending = live(SessionProbe::LoggedOut, Some("uuid-new"));
        assert_eq!(
            resolve(&reg, &pending, NOW, true),
            LiveSession::Unsaved {
                account_uuid: Some("uuid-new".into())
            }
        );
        assert_eq!(
            resolve(&reg, &live(SessionProbe::LoggedOut, None), NOW, true),
            LiveSession::LoggedOut
        );
    }

    #[test]
    fn an_unreadable_cookie_store_falls_back_on_the_recorded_id() {
        let reg = registry(&[("a", Some("uuid-a"))], None);
        assert_eq!(
            resolve(
                &reg,
                &live(SessionProbe::Unknown, Some("uuid-a")),
                NOW,
                false
            ),
            saved("a")
        );
        assert_eq!(
            resolve(&reg, &live(SessionProbe::Unknown, None), NOW, false),
            LiveSession::LoggedOut
        );
    }

    #[test]
    fn an_interrupted_rewrite_overrides_everything() {
        let mut reg = registry(&[("a", Some("uuid-a"))], Some("a"));
        reg.rewrite_in_progress = true;
        assert_eq!(
            resolve(&reg, &live(LOGGED_IN, Some("uuid-a")), NOW, false),
            LiveSession::Interrupted
        );
    }

    #[test]
    fn unnamed_labels_are_numbered() {
        let mut reg = registry(&[], None);
        assert_eq!(unused_label(&reg), UNNAMED_LABEL);
        for expected in [
            UNNAMED_LABEL.to_owned(),
            format!("{UNNAMED_LABEL} 2"),
            format!("{UNNAMED_LABEL} 3"),
        ] {
            let label = unused_label(&reg);
            assert_eq!(label, expected);
            reg.accounts.push(Account {
                id: label.clone(),
                label,
                account_uuid: None,
                created_at: 0,
                saved_at: 0,
                last_used_at: None,
            });
        }
    }
}
