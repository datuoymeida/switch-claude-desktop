//! Reading and patching Claude Desktop's `config.json`.
//!
//! Besides ordinary settings, the file holds per-account state: the OS-encrypted OAuth
//! token caches (`oauth:*`) and `lastKnownAccountUuid`. Those keys travel with a saved
//! account; every other key is left alone when switching.
//!
//! Claude writes the file through a write-ahead journal, `config.json.journal`:
//!
//! ```text
//! <sha256 of the new content> <sha256 of the config.json it replaces> .
//! <new content>
//! ```
//!
//! A journal left behind when the app quits is newer than `config.json`, so it is the
//! state to read. After patching, the merged result is written to `config.json` and the
//! journal is removed, which leaves a single source of truth for the next launch.

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::path::Path;

use crate::fsutil;

pub const CONFIG_FILE: &str = "config.json";
pub const JOURNAL_FILE: &str = "config.json.journal";
pub const ACCOUNT_UUID_KEY: &str = "lastKnownAccountUuid";
const OAUTH_PREFIX: &str = "oauth:";

pub type ConfigMap = Map<String, Value>;

/// Keys of `config.json` that belong to the signed-in account rather than to the app.
pub fn is_account_key(key: &str) -> bool {
    key == ACCOUNT_UUID_KEY || key.starts_with(OAUTH_PREFIX)
}

/// The per-account entries of `config`.
pub fn account_keys(config: &ConfigMap) -> ConfigMap {
    config
        .iter()
        .filter(|(key, _)| is_account_key(key))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

/// The account Claude last saw signed in, if it recorded one.
pub fn account_uuid(config: &ConfigMap) -> Option<String> {
    config
        .get(ACCOUNT_UUID_KEY)?
        .as_str()
        .filter(|uuid| !uuid.is_empty())
        .map(str::to_owned)
}

/// The configuration Claude would load on its next start: the pending journal when one
/// applies, otherwise `config.json`. `None` when neither exists.
pub fn read_effective(data_dir: &Path) -> Result<Option<ConfigMap>> {
    let config_path = data_dir.join(CONFIG_FILE);
    let config_bytes = fsutil::read_optional(&config_path)?;
    // An unreadable journal is no reason to fail: `config.json` is still a valid state.
    let journal_bytes = fsutil::read_optional(&data_dir.join(JOURNAL_FILE)).unwrap_or(None);

    if let Some(journal) = &journal_bytes
        && let Some(pending) = pending_journal(journal, config_bytes.as_deref())
    {
        return Ok(Some(pending));
    }
    let Some(bytes) = config_bytes else {
        return Ok(None);
    };
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(Value::Object(map)) => Ok(Some(map)),
        Ok(_) => bail!("{} không chứa một đối tượng JSON", config_path.display()),
        Err(e) => Err(e).with_context(|| format!("Không đọc được {}", config_path.display())),
    }
}

/// Replace the per-account entries of Claude's configuration with `keys` (pass an empty
/// map to sign the configuration out). All other settings are preserved.
pub fn replace_account_keys(data_dir: &Path, keys: &ConfigMap) -> Result<()> {
    let existing = read_effective(data_dir)?;
    if existing.is_none() && !keys.keys().any(|key| is_account_key(key)) {
        return Ok(()); // nothing to sign out and nothing to add
    }
    let mut config = existing.unwrap_or_default();
    config.retain(|key, _| !is_account_key(key));
    for (key, value) in keys {
        if is_account_key(key) {
            config.insert(key.clone(), value.clone());
        }
    }

    fsutil::write_atomic(&data_dir.join(CONFIG_FILE), &to_claude_json(&config)?)?;
    // The journal described a transition from the old file; it no longer applies.
    fsutil::remove_any(&data_dir.join(JOURNAL_FILE))
}

/// The content of a journal, if it is intact and applies to the current `config.json`.
fn pending_journal(journal: &[u8], config: Option<&[u8]>) -> Option<ConfigMap> {
    let newline = journal.iter().position(|b| *b == b'\n')?;
    let header = std::str::from_utf8(&journal[..newline]).ok()?;
    let body = &journal[newline + 1..];

    let mut fields = header.split_ascii_whitespace();
    let new_hash = fields.next()?;
    let base_hash = fields.next()?;
    if !sha256_hex(body).eq_ignore_ascii_case(new_hash) {
        return None; // torn or tampered journal
    }
    if let Some(config) = config
        && !sha256_hex(config).eq_ignore_ascii_case(base_hash)
    {
        return None; // written against a different config.json (or already applied)
    }
    match serde_json::from_slice::<Value>(body).ok()? {
        Value::Object(map) => Some(map),
        _ => None,
    }
}

/// Serialize the way Claude does: tab-indented, key order preserved, no trailing newline.
fn to_claude_json(config: &ConfigMap) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b"\t");
    let mut serializer = serde_json::Serializer::with_formatter(&mut out, formatter);
    config
        .serialize(&mut serializer)
        .context("Không tạo được nội dung config.json")?;
    Ok(out)
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    fn map(value: Value) -> ConfigMap {
        value.as_object().unwrap().clone()
    }

    fn write_journal(dir: &Path, base: &[u8], body: &[u8]) {
        let mut journal = format!("{} {} .\n", sha256_hex(body), sha256_hex(base)).into_bytes();
        journal.extend_from_slice(body);
        fs::write(dir.join(JOURNAL_FILE), journal).unwrap();
    }

    #[test]
    fn hashes_match_a_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn classifies_account_keys() {
        assert!(is_account_key("oauth:tokenCache"));
        assert!(is_account_key("oauth:tokenCacheV2"));
        assert!(is_account_key("lastKnownAccountUuid"));
        assert!(!is_account_key("locale"));
        assert!(!is_account_key("dxt:allowlistEnabled:1234"));

        let config =
            map(json!({"locale": "vi", "oauth:tokenCache": "x", "lastKnownAccountUuid": "u-1"}));
        assert_eq!(
            account_keys(&config),
            map(json!({"oauth:tokenCache": "x", "lastKnownAccountUuid": "u-1"}))
        );
        assert_eq!(account_uuid(&config).as_deref(), Some("u-1"));
        assert_eq!(
            account_uuid(&map(json!({"lastKnownAccountUuid": ""}))),
            None
        );
        assert_eq!(
            account_uuid(&map(json!({"lastKnownAccountUuid": null}))),
            None
        );
    }

    #[test]
    fn missing_files_mean_no_config() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(read_effective(tmp.path()).unwrap().is_none());
    }

    #[test]
    fn broken_config_is_an_error_not_an_empty_config() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join(CONFIG_FILE), b"{ not json").unwrap();
        assert!(read_effective(tmp.path()).is_err());
        // And patching must refuse to overwrite a file it could not understand.
        assert!(replace_account_keys(tmp.path(), &ConfigMap::new()).is_err());
        assert_eq!(
            fs::read(tmp.path().join(CONFIG_FILE)).unwrap(),
            b"{ not json"
        );
    }

    #[test]
    fn a_pending_journal_takes_precedence() {
        let tmp = tempfile::tempdir().unwrap();
        let base = br#"{"locale":"en","oauth:tokenCache":"old"}"#;
        let newer = br#"{"locale":"en","oauth:tokenCache":"new"}"#;
        fs::write(tmp.path().join(CONFIG_FILE), base).unwrap();
        write_journal(tmp.path(), base, newer);

        let config = read_effective(tmp.path()).unwrap().unwrap();
        assert_eq!(config["oauth:tokenCache"], "new");
    }

    #[test]
    fn a_journal_for_another_base_is_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join(CONFIG_FILE), br#"{"v":"current"}"#).unwrap();
        write_journal(
            tmp.path(),
            br#"{"v":"something else"}"#,
            br#"{"v":"stale"}"#,
        );
        assert_eq!(read_effective(tmp.path()).unwrap().unwrap()["v"], "current");
    }

    #[test]
    fn a_torn_journal_is_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let base = br#"{"v":"current"}"#;
        fs::write(tmp.path().join(CONFIG_FILE), base).unwrap();
        write_journal(tmp.path(), base, br#"{"v":"newer"}"#);
        // Chop the end off, as an interrupted write would.
        let path = tmp.path().join(JOURNAL_FILE);
        let mut bytes = fs::read(&path).unwrap();
        bytes.truncate(bytes.len() - 3);
        fs::write(&path, bytes).unwrap();
        assert_eq!(read_effective(tmp.path()).unwrap().unwrap()["v"], "current");

        fs::write(&path, b"no header line at all").unwrap();
        assert_eq!(read_effective(tmp.path()).unwrap().unwrap()["v"], "current");
    }

    #[test]
    fn a_journal_alone_is_still_read() {
        let tmp = tempfile::tempdir().unwrap();
        write_journal(tmp.path(), b"", br#"{"v":"only in journal"}"#);
        assert_eq!(
            read_effective(tmp.path()).unwrap().unwrap()["v"],
            "only in journal"
        );
    }

    #[test]
    fn replacing_account_keys_keeps_everything_else() {
        let tmp = tempfile::tempdir().unwrap();
        let base =
            br#"{"locale":"en","oauth:tokenCache":"A1","lastKnownAccountUuid":"uuid-a","scale":0}"#;
        // The journal carries a newer token for A and a setting changed since.
        let newer = br#"{"locale":"vi","oauth:tokenCache":"A2","oauth:tokenCacheV2":"A2v2","lastKnownAccountUuid":"uuid-a","scale":0}"#;
        fs::write(tmp.path().join(CONFIG_FILE), base).unwrap();
        write_journal(tmp.path(), base, newer);

        let b_keys = map(json!({
            "oauth:tokenCache": "B1",
            "lastKnownAccountUuid": "uuid-b",
            "locale": "should be ignored: not an account key",
        }));
        replace_account_keys(tmp.path(), &b_keys).unwrap();

        assert!(!tmp.path().join(JOURNAL_FILE).exists());
        let config = read_effective(tmp.path()).unwrap().unwrap();
        assert_eq!(
            config,
            map(json!({
                "locale": "vi",
                "scale": 0,
                "oauth:tokenCache": "B1",
                "lastKnownAccountUuid": "uuid-b",
            }))
        );
        // Settings keep their original order; account keys follow.
        let order: Vec<_> = config.keys().map(String::as_str).collect();
        assert_eq!(
            order,
            [
                "locale",
                "scale",
                "oauth:tokenCache",
                "lastKnownAccountUuid"
            ]
        );
    }

    #[test]
    fn an_empty_key_set_signs_the_config_out() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join(CONFIG_FILE),
            br#"{"locale":"en","oauth:tokenCache":"A","lastKnownAccountUuid":"uuid-a"}"#,
        )
        .unwrap();
        replace_account_keys(tmp.path(), &ConfigMap::new()).unwrap();
        let config = read_effective(tmp.path()).unwrap().unwrap();
        assert_eq!(config, map(json!({"locale": "en"})));
    }

    #[test]
    fn output_matches_claudes_formatting() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join(CONFIG_FILE),
            br#"{"a":{"b":[1,2]},"c":"d"}"#,
        )
        .unwrap();
        replace_account_keys(tmp.path(), &ConfigMap::new()).unwrap();
        let text = fs::read_to_string(tmp.path().join(CONFIG_FILE)).unwrap();
        assert_eq!(
            text,
            "{\n\t\"a\": {\n\t\t\"b\": [\n\t\t\t1,\n\t\t\t2\n\t\t]\n\t},\n\t\"c\": \"d\"\n}"
        );
    }

    #[test]
    fn creates_a_config_when_none_exists() {
        let tmp = tempfile::tempdir().unwrap();
        replace_account_keys(tmp.path(), &map(json!({"oauth:tokenCache": "B"}))).unwrap();
        let config = read_effective(tmp.path()).unwrap().unwrap();
        assert_eq!(config, map(json!({"oauth:tokenCache": "B"})));
    }
}
