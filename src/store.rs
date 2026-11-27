//! This tool's own storage: the list of saved accounts and one snapshot per account.
//!
//! ```text
//! <store>/accounts.json            the registry below
//! <store>/profiles/<id>/files/     session entries copied from Claude's data directory
//! <store>/profiles/<id>/keys.json  the account's entries of Claude's config.json
//! ```
//!
//! Everything sensitive in here (cookie values, OAuth tokens) is still encrypted with the
//! OS-held key Claude itself uses; the directory is additionally owner-only on Unix.

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config_json::ConfigMap;
use crate::fsutil;

const REGISTRY_FILE: &str = "accounts.json";
const PROFILES_DIR: &str = "profiles";
const FILES_DIR: &str = "files";
const KEYS_FILE: &str = "keys.json";
const LOCK_FILE: &str = ".lock";
const REGISTRY_VERSION: u32 = 1;

/// Longest label accepted for an account, in characters.
pub const MAX_LABEL_CHARS: usize = 60;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    /// Name chosen by the user. Claude keeps no readable e-mail address on disk.
    pub label: String,
    /// Claude's own identifier for the account, used to recognise the live session.
    #[serde(default)]
    pub account_uuid: Option<String>,
    pub created_at: i64,
    /// When the snapshot was last refreshed from the live session.
    pub saved_at: i64,
    #[serde(default)]
    pub last_used_at: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default)]
    pub version: u32,
    /// The account whose session was last put into Claude by this tool.
    #[serde(default)]
    pub active_id: Option<String>,
    /// Set while the live session is being rewritten. If it is still set later, that
    /// rewrite was interrupted and the live session must not be saved as anyone's.
    #[serde(default)]
    pub rewrite_in_progress: bool,
    #[serde(default)]
    pub accounts: Vec<Account>,
}

impl Registry {
    pub fn find(&self, id: &str) -> Option<&Account> {
        self.accounts.iter().find(|a| a.id == id)
    }

    pub fn find_mut(&mut self, id: &str) -> Option<&mut Account> {
        self.accounts.iter_mut().find(|a| a.id == id)
    }

    pub fn find_by_uuid(&self, uuid: &str) -> Option<&Account> {
        self.accounts
            .iter()
            .find(|a| a.account_uuid.as_deref() == Some(uuid))
    }

    pub fn active(&self) -> Option<&Account> {
        self.active_id.as_deref().and_then(|id| self.find(id))
    }
}

/// Trim a user-supplied label and check that it is usable.
pub fn normalize_label(label: &str) -> Result<String> {
    let label = label.split_whitespace().collect::<Vec<_>>().join(" ");
    ensure!(!label.is_empty(), "Hãy nhập tên cho tài khoản.");
    ensure!(
        label.chars().count() <= MAX_LABEL_CHARS,
        "Tên tài khoản dài tối đa {MAX_LABEL_CHARS} ký tự."
    );
    Ok(label)
}

pub struct Store {
    root: PathBuf,
}

/// Proof that this process owns the store. The claim ends when the value is dropped.
pub struct StoreLock(#[allow(dead_code)] std::fs::File);

impl Store {
    pub fn open(root: PathBuf) -> Result<Self> {
        fsutil::create_private_dir(&root)?;
        fsutil::create_private_dir(&root.join(PROFILES_DIR))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Claim the store for this process. Fails when another copy of the app holds it:
    /// two copies could otherwise rewrite Claude's session at the same time.
    pub fn lock(&self) -> Result<StoreLock> {
        let path = self.root.join(LOCK_FILE);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("Không mở được {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => Ok(StoreLock(file)),
            Err(std::fs::TryLockError::WouldBlock) => {
                bail!("Ứng dụng này đang mở ở một cửa sổ khác. Hãy dùng cửa sổ đó.")
            }
            Err(std::fs::TryLockError::Error(error)) => {
                Err(error).with_context(|| format!("Không khoá được {}", path.display()))
            }
        }
    }

    pub fn load(&self) -> Result<Registry> {
        let path = self.root.join(REGISTRY_FILE);
        let Some(bytes) = fsutil::read_optional(&path)? else {
            return Ok(Registry {
                version: REGISTRY_VERSION,
                ..Registry::default()
            });
        };
        let registry: Registry = serde_json::from_slice(&bytes).with_context(|| {
            format!(
                "Danh sách tài khoản bị hỏng, không đọc được: {}",
                path.display()
            )
        })?;
        ensure!(
            registry.version <= REGISTRY_VERSION,
            "Danh sách tài khoản được tạo bởi phiên bản mới hơn của ứng dụng này."
        );
        Ok(registry)
    }

    pub fn save(&self, registry: &Registry) -> Result<()> {
        let mut registry = registry.clone();
        registry.version = REGISTRY_VERSION;
        let bytes = serde_json::to_vec_pretty(&registry).context("Không ghi được danh sách")?;
        fsutil::write_atomic(&self.root.join(REGISTRY_FILE), &bytes)
    }

    /// A fresh account id: unique, sortable by creation time and safe as a folder name.
    pub fn new_id(&self) -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let mut id = format!("acc-{nanos:x}");
        while self.root.join(PROFILES_DIR).join(&id).exists() {
            id.push('x');
        }
        id
    }

    fn profile_dir(&self, id: &str) -> Result<PathBuf> {
        // Ids come from accounts.json; never let one escape the profiles directory.
        let safe = !id.is_empty()
            && id.len() <= 80
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !safe {
            bail!("Mã tài khoản không hợp lệ: {id:?}");
        }
        Ok(self.root.join(PROFILES_DIR).join(id))
    }

    /// Where the session entries of account `id` are kept.
    pub fn files_dir(&self, id: &str) -> Result<PathBuf> {
        Ok(self.profile_dir(id)?.join(FILES_DIR))
    }

    pub fn has_snapshot(&self, id: &str) -> bool {
        self.files_dir(id).is_ok_and(|dir| dir.is_dir())
    }

    pub fn write_keys(&self, id: &str, keys: &ConfigMap) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(keys).context("Không ghi được khoá tài khoản")?;
        fsutil::write_atomic(&self.profile_dir(id)?.join(KEYS_FILE), &bytes)
    }

    pub fn read_keys(&self, id: &str) -> Result<ConfigMap> {
        let path = self.profile_dir(id)?.join(KEYS_FILE);
        let Some(bytes) = fsutil::read_optional(&path)? else {
            return Ok(ConfigMap::new());
        };
        match serde_json::from_slice::<Value>(&bytes) {
            Ok(Value::Object(map)) => Ok(map),
            _ => bail!("Dữ liệu tài khoản bị hỏng: {}", path.display()),
        }
    }

    pub fn remove_profile(&self, id: &str) -> Result<()> {
        fsutil::remove_any(&self.profile_dir(id)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn account(id: &str, label: &str, uuid: Option<&str>) -> Account {
        Account {
            id: id.into(),
            label: label.into(),
            account_uuid: uuid.map(Into::into),
            created_at: 1,
            saved_at: 2,
            last_used_at: None,
        }
    }

    #[test]
    fn registry_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path().join("store")).unwrap();
        assert_eq!(store.load().unwrap().accounts, []);

        let mut registry = store.load().unwrap();
        registry
            .accounts
            .push(account("acc-1", "Công việc", Some("uuid-1")));
        registry.accounts.push(account("acc-2", "Cá nhân", None));
        registry.active_id = Some("acc-2".into());
        store.save(&registry).unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(loaded, registry);
        assert_eq!(loaded.active().unwrap().label, "Cá nhân");
        assert_eq!(loaded.find_by_uuid("uuid-1").unwrap().id, "acc-1");
        assert!(loaded.find_by_uuid("uuid-9").is_none());
    }

    #[test]
    fn older_registry_files_still_load() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path().to_path_buf()).unwrap();
        std::fs::write(
            tmp.path().join(REGISTRY_FILE),
            br#"{"accounts":[{"id":"acc-1","label":"A","created_at":1,"saved_at":2}]}"#,
        )
        .unwrap();
        let registry = store.load().unwrap();
        assert_eq!(registry.accounts, [account("acc-1", "A", None)]);
        assert!(!registry.rewrite_in_progress);
    }

    #[test]
    fn a_corrupt_registry_is_reported_not_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path().to_path_buf()).unwrap();
        std::fs::write(tmp.path().join(REGISTRY_FILE), b"{oops").unwrap();
        assert!(store.load().is_err());
        assert_eq!(
            std::fs::read(tmp.path().join(REGISTRY_FILE)).unwrap(),
            b"{oops"
        );
    }

    #[test]
    fn keys_round_trip_and_default_to_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path().to_path_buf()).unwrap();
        assert!(store.read_keys("acc-1").unwrap().is_empty());
        let keys = json!({"oauth:tokenCache": "abc", "lastKnownAccountUuid": "u"});
        let keys = keys.as_object().unwrap().clone();
        store.write_keys("acc-1", &keys).unwrap();
        assert_eq!(store.read_keys("acc-1").unwrap(), keys);

        store.remove_profile("acc-1").unwrap();
        assert!(store.read_keys("acc-1").unwrap().is_empty());
    }

    #[test]
    fn ids_cannot_escape_the_store() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path().to_path_buf()).unwrap();
        for bad in ["", "..", "../x", "a/b", "a\\b", "C:evil"] {
            assert!(store.files_dir(bad).is_err(), "{bad:?} should be rejected");
            assert!(store.remove_profile(bad).is_err());
            assert!(!store.has_snapshot(bad));
        }
        let id = store.new_id();
        assert!(store.files_dir(&id).unwrap().starts_with(tmp.path()));
    }

    #[test]
    fn new_ids_do_not_collide_with_existing_profiles() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path().to_path_buf()).unwrap();
        let first = store.new_id();
        std::fs::create_dir_all(store.files_dir(&first).unwrap()).unwrap();
        assert!(store.has_snapshot(&first));
        assert_ne!(store.new_id(), first);
    }

    #[test]
    fn only_one_process_can_hold_the_store() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path().to_path_buf()).unwrap();
        let first = store.lock().unwrap();
        // A second claim (here: a second handle, as another process would have) is refused.
        let other = Store::open(tmp.path().to_path_buf()).unwrap();
        let error = other.lock().err().expect("the store is already held");
        assert!(
            error.to_string().contains("đang mở ở một cửa sổ khác"),
            "{error}"
        );
        drop(first);
        other.lock().unwrap();
    }

    #[test]
    fn labels_are_normalized() {
        assert_eq!(normalize_label("  Công   việc \n").unwrap(), "Công việc");
        assert!(normalize_label("   ").is_err());
        assert!(normalize_label(&"x".repeat(MAX_LABEL_CHARS)).is_ok());
        assert!(normalize_label(&"x".repeat(MAX_LABEL_CHARS + 1)).is_err());
    }
}
