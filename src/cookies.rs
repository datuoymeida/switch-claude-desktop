//! Read-only access to the metadata of Chromium's `Cookies` database.
//!
//! Claude Desktop is an Electron app, so its login session is a `sessionKey` cookie for
//! claude.ai kept in a SQLite file. To tell whether a profile is logged in, and until
//! when, only each cookie's host, name and expiry are needed. Those are stored in the
//! clear; the cookie *values* are encrypted by the OS and are never decoded here.
//!
//! The file is a few dozen kilobytes holding one small table, so instead of linking
//! SQLite this module walks the table b-tree directly
//! (format: <https://www.sqlite.org/fileformat2.html>).

use anyhow::{Context, Result, bail, ensure};
use std::borrow::Cow;
use std::path::{Path, PathBuf};

/// Chromium stores times as microseconds since 1601-01-01 UTC.
const CHROMIUM_TO_UNIX_SECS: i64 = 11_644_473_600;
/// Refuse to load anything implausibly large for a cookie store.
const MAX_DB_BYTES: u64 = 64 * 1024 * 1024;
const HEADER_LEN: usize = 100;

const PAGE_TABLE_INTERIOR: u8 = 0x05;
const PAGE_TABLE_LEAF: u8 = 0x0d;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CookieMeta {
    pub host: String,
    pub name: String,
    /// Expiry in Unix seconds; `None` for a cookie without an expiry date.
    pub expires_unix: Option<i64>,
}

/// Login state found in a profile's cookie store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionProbe {
    /// A claude.ai session cookie exists.
    LoggedIn { expires_unix: Option<i64> },
    /// There is no session cookie, or no cookie store at all.
    LoggedOut,
    /// The cookie store could not be read, for example because it is mid-write.
    Unknown,
}

impl SessionProbe {
    pub fn is_expired(&self, now_unix: i64) -> bool {
        matches!(self, SessionProbe::LoggedIn { expires_unix: Some(t) } if *t <= now_unix)
    }

    /// Logged in with a session that has not expired.
    pub fn is_active(&self, now_unix: i64) -> bool {
        matches!(self, SessionProbe::LoggedIn { .. }) && !self.is_expired(now_unix)
    }
}

/// The cookie database inside a Claude data directory (or a snapshot of one).
/// Chromium keeps it under `Network/` on Windows and at the top level elsewhere.
pub fn cookie_db_path(dir: &Path) -> Option<PathBuf> {
    [dir.join("Network").join("Cookies"), dir.join("Cookies")]
        .into_iter()
        .find(|p| p.is_file())
}

/// Inspect the cookie store of a Claude data directory (or a snapshot of one).
pub fn probe_session(dir: &Path) -> SessionProbe {
    let Some(db) = cookie_db_path(dir) else {
        return SessionProbe::LoggedOut;
    };
    // Chromium empties the rollback journal when a write is complete. While the journal
    // has content the database is being rewritten, and what it holds at this instant may
    // be neither the old state nor the new one. Checked before and after reading it.
    let mid_write = || {
        let mut journal = db.clone().into_os_string();
        journal.push("-journal");
        std::fs::metadata(journal).is_ok_and(|journal| journal.len() > 0)
    };
    if mid_write() {
        return SessionProbe::Unknown;
    }
    let cookies = read_cookie_meta(&db);
    if mid_write() {
        return SessionProbe::Unknown;
    }
    match cookies {
        Ok(cookies) => find_session(&cookies),
        Err(_) => SessionProbe::Unknown,
    }
}

/// Look for the claude.ai login cookie among `cookies`.
pub fn find_session(cookies: &[CookieMeta]) -> SessionProbe {
    let mut found = false;
    let mut latest: Option<i64> = None;
    for cookie in cookies.iter().filter(|c| is_session_cookie(c)) {
        found = true;
        latest = latest.max(cookie.expires_unix);
    }
    if found {
        SessionProbe::LoggedIn {
            expires_unix: latest,
        }
    } else {
        SessionProbe::LoggedOut
    }
}

fn is_session_cookie(cookie: &CookieMeta) -> bool {
    let host = cookie.host.trim_start_matches('.');
    let on_claude = host == "claude.ai" || host.ends_with(".claude.ai");
    // `sessionKey` is the login cookie; newer builds add versioned variants such as
    // `sessionKeyV3`. Their `…LC` companions are script-readable markers, not credentials.
    on_claude && cookie.name.starts_with("sessionKey") && !cookie.name.ends_with("LC")
}

pub fn read_cookie_meta(path: &Path) -> Result<Vec<CookieMeta>> {
    let len = std::fs::metadata(path)
        .with_context(|| format!("cannot stat {}", path.display()))?
        .len();
    ensure!(
        len <= MAX_DB_BYTES,
        "cookie database is unexpectedly large ({len} bytes)"
    );
    let bytes = std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    parse_cookie_meta(&bytes)
}

/// Parse the raw bytes of a `Cookies` database.
pub fn parse_cookie_meta(bytes: &[u8]) -> Result<Vec<CookieMeta>> {
    let db = Db::open(bytes)?;
    let Some((root, create_sql)) = db.find_table("cookies")? else {
        return Ok(Vec::new());
    };

    // Column positions come from the schema; the fallbacks match Chromium's layout.
    let columns = column_names(&create_sql);
    let index_of =
        |name: &str, fallback: usize| columns.iter().position(|c| c == name).unwrap_or(fallback);
    let host_col = index_of("host_key", 1);
    let name_col = index_of("name", 3);
    let expires_col = index_of("expires_utc", 7);

    let mut cookies = Vec::new();
    db.for_each_row(root, &mut |payload| {
        let values = decode_record(payload)?;
        let (Some(host), Some(name)) = (text_at(&values, host_col), text_at(&values, name_col))
        else {
            return Ok(());
        };
        let expires_unix = int_at(&values, expires_col)
            .filter(|micros| *micros > 0)
            .map(|micros| micros / 1_000_000 - CHROMIUM_TO_UNIX_SECS);
        cookies.push(CookieMeta {
            host,
            name,
            expires_unix,
        });
        Ok(())
    })?;
    Ok(cookies)
}

/// A SQLite database image held in memory.
struct Db<'a> {
    bytes: &'a [u8],
    page_size: usize,
    /// Bytes per page that hold content (page size minus the reserved tail).
    usable: usize,
    page_count: u32,
}

impl<'a> Db<'a> {
    fn open(bytes: &'a [u8]) -> Result<Self> {
        ensure!(
            bytes.len() >= HEADER_LEN && bytes.starts_with(b"SQLite format 3\0"),
            "not a SQLite database"
        );
        let raw_page_size = u16::from_be_bytes([bytes[16], bytes[17]]) as usize;
        let page_size = if raw_page_size == 1 {
            65536
        } else {
            raw_page_size
        };
        ensure!(
            page_size >= 512 && page_size.is_power_of_two(),
            "invalid page size {page_size}"
        );
        let reserved = bytes[20] as usize;
        ensure!(
            page_size - reserved >= 480,
            "invalid reserved space {reserved}"
        );
        // 1 = UTF-8; 0 only appears in a database with no tables yet.
        let encoding = be32(bytes, 56)?;
        ensure!(encoding <= 1, "unsupported text encoding {encoding}");
        let page_count = u32::try_from(bytes.len() / page_size).unwrap_or(u32::MAX);
        ensure!(page_count >= 1, "database is shorter than one page");
        Ok(Self {
            bytes,
            page_size,
            usable: page_size - reserved,
            page_count,
        })
    }

    /// Pages are numbered from 1.
    fn page(&self, number: u32) -> Result<&'a [u8]> {
        ensure!(
            number >= 1 && number <= self.page_count,
            "page {number} is outside the file"
        );
        let start = (number as usize - 1) * self.page_size;
        Ok(&self.bytes[start..start + self.page_size])
    }

    /// Root page and `CREATE TABLE` statement of `table`, from the schema table on page 1.
    fn find_table(&self, table: &str) -> Result<Option<(u32, String)>> {
        let mut found = None;
        self.for_each_row(1, &mut |payload| {
            // Schema rows are (type, name, tbl_name, rootpage, sql).
            let values = decode_record(payload)?;
            if text_at(&values, 0).as_deref() == Some("table")
                && text_at(&values, 1).as_deref() == Some(table)
                && let Some(root) = int_at(&values, 3).and_then(|r| u32::try_from(r).ok())
            {
                found = Some((root, text_at(&values, 4).unwrap_or_default()));
            }
            Ok(())
        })?;
        Ok(found)
    }

    /// Call `on_row` with the record payload of every row in the table rooted at `root`.
    fn for_each_row(&self, root: u32, on_row: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<()> {
        let mut pending = vec![root];
        let mut visited: u32 = 0;
        while let Some(number) = pending.pop() {
            // A well-formed tree visits each page at most once.
            visited += 1;
            ensure!(visited <= self.page_count, "b-tree contains a cycle");

            let page = self.page(number)?;
            // Page 1 starts with the 100-byte file header.
            let base = if number == 1 { HEADER_LEN } else { 0 };
            let kind = page[base];
            let cell_count = be16(page, base + 3)? as usize;
            match kind {
                PAGE_TABLE_INTERIOR => {
                    pending.push(be32(page, base + 8)?);
                    for i in 0..cell_count {
                        let cell = be16(page, base + 12 + 2 * i)? as usize;
                        pending.push(be32(page, cell)?);
                    }
                }
                PAGE_TABLE_LEAF => {
                    for i in 0..cell_count {
                        let cell = be16(page, base + 8 + 2 * i)? as usize;
                        let (payload_len, a) = varint(page, cell)?;
                        let (_rowid, b) = varint(page, cell + a)?;
                        let payload = self.payload(page, cell + a + b, payload_len)?;
                        on_row(&payload)?;
                    }
                }
                other => bail!("unexpected b-tree page type {other:#04x}"),
            }
        }
        Ok(())
    }

    /// Assemble a cell's payload, following overflow pages when it does not fit locally.
    fn payload(&self, page: &'a [u8], start: usize, total: u64) -> Result<Cow<'a, [u8]>> {
        let total = usize::try_from(total)
            .ok()
            .filter(|t| *t <= self.bytes.len())
            .context("payload is larger than the file")?;
        let max_local = self.usable - 35;
        if total <= max_local {
            return page
                .get(start..start + total)
                .map(Cow::Borrowed)
                .context("cell extends past its page");
        }

        let min_local = (self.usable - 12) * 32 / 255 - 23;
        let k = min_local + (total - min_local) % (self.usable - 4);
        let local = if k <= max_local { k } else { min_local };
        let mut data = Vec::with_capacity(total);
        data.extend_from_slice(
            page.get(start..start + local)
                .context("cell extends past its page")?,
        );
        let mut next = be32(page, start + local)?;
        let mut hops: u32 = 0;
        while data.len() < total {
            ensure!(next != 0, "overflow chain ends early");
            hops += 1;
            ensure!(hops <= self.page_count, "overflow chain contains a cycle");
            let overflow = self.page(next)?;
            let take = (total - data.len()).min(self.usable - 4);
            data.extend_from_slice(&overflow[4..4 + take]);
            next = be32(overflow, 0)?;
        }
        Ok(Cow::Owned(data))
    }
}

/// A decoded record field. Blob and float contents are never needed, so they are dropped.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Val<'p> {
    Null,
    Int(i64),
    Float,
    Text(&'p [u8]),
    Blob,
}

fn decode_record(payload: &[u8]) -> Result<Vec<Val<'_>>> {
    let (header_len, first) = varint(payload, 0)?;
    let header_len = usize::try_from(header_len)
        .ok()
        .filter(|h| *h >= first && *h <= payload.len())
        .context("invalid record header")?;

    let mut values = Vec::new();
    let mut type_pos = first;
    let mut body = header_len;
    while type_pos < header_len {
        let (serial, used) = varint(payload, type_pos)?;
        type_pos += used;
        let size = match serial {
            0 | 8 | 9 => 0,
            1 => 1,
            2 => 2,
            3 => 3,
            4 => 4,
            5 => 6,
            6 | 7 => 8,
            10 | 11 => bail!("reserved serial type {serial}"),
            n => usize::try_from((n - 12) / 2).context("field is too large")?,
        };
        let end = body.checked_add(size).context("field is too large")?;
        let field = payload.get(body..end).context("record body is truncated")?;
        values.push(match serial {
            0 => Val::Null,
            1..=6 => Val::Int(signed_be(field)),
            7 => Val::Float,
            8 => Val::Int(0),
            9 => Val::Int(1),
            n if n % 2 == 0 => Val::Blob,
            _ => Val::Text(field),
        });
        body = end;
    }
    Ok(values)
}

fn text_at(values: &[Val<'_>], index: usize) -> Option<String> {
    match values.get(index) {
        Some(Val::Text(bytes)) => Some(String::from_utf8_lossy(bytes).into_owned()),
        _ => None,
    }
}

fn int_at(values: &[Val<'_>], index: usize) -> Option<i64> {
    match values.get(index) {
        Some(Val::Int(v)) => Some(*v),
        _ => None,
    }
}

/// Column names of a `CREATE TABLE` statement, in declaration order.
fn column_names(create_sql: &str) -> Vec<String> {
    let (Some(open), Some(close)) = (create_sql.find('('), create_sql.rfind(')')) else {
        return Vec::new();
    };
    if close <= open {
        return Vec::new();
    }
    let body = &create_sql[open + 1..close];

    let mut names = Vec::new();
    let mut add = |definition: &str| {
        let token = definition
            .trim()
            .split(|c: char| c.is_whitespace() || c == '(')
            .next()
            .unwrap_or("")
            .trim_matches(|c| matches!(c, '"' | '`' | '\'' | '[' | ']'));
        let is_constraint = matches!(
            token.to_ascii_uppercase().as_str(),
            "CONSTRAINT" | "PRIMARY" | "UNIQUE" | "CHECK" | "FOREIGN"
        );
        if !token.is_empty() && !is_constraint {
            names.push(token.to_owned());
        }
    };

    let mut depth = 0usize;
    let mut start = 0usize;
    for (i, ch) in body.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                add(&body[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    add(&body[start..]);
    names
}

/// SQLite varint: big-endian, 7 bits per byte, except a ninth byte that carries 8 bits.
fn varint(buf: &[u8], pos: usize) -> Result<(u64, usize)> {
    let mut value: u64 = 0;
    for i in 0..9 {
        let byte = *buf
            .get(pos.checked_add(i).context("varint position overflow")?)
            .context("varint runs past the buffer")?;
        if i == 8 {
            return Ok(((value << 8) | u64::from(byte), 9));
        }
        value = (value << 7) | u64::from(byte & 0x7f);
        if byte & 0x80 == 0 {
            return Ok((value, i + 1));
        }
    }
    unreachable!("the loop returns by its ninth iteration")
}

fn signed_be(bytes: &[u8]) -> i64 {
    let negative = bytes.first().is_some_and(|b| b & 0x80 != 0);
    let mut value: i64 = if negative { -1 } else { 0 };
    for byte in bytes {
        value = (value << 8) | i64::from(*byte);
    }
    value
}

fn be16(buf: &[u8], pos: usize) -> Result<u16> {
    let bytes = buf
        .get(pos..pos + 2)
        .context("read past the end of a page")?;
    Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
}

fn be32(buf: &[u8], pos: usize) -> Result<u32> {
    let bytes = buf
        .get(pos..pos + 4)
        .context("read past the end of a page")?;
    Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOGGED_IN_A: &[u8] = include_bytes!("../tests/fixtures/cookies_logged_in_a.db");
    const LOGGED_IN_B: &[u8] = include_bytes!("../tests/fixtures/cookies_logged_in_b.db");
    const LOGGED_OUT: &[u8] = include_bytes!("../tests/fixtures/cookies_logged_out.db");
    const EXPIRED: &[u8] = include_bytes!("../tests/fixtures/cookies_expired.db");
    const LARGE: &[u8] = include_bytes!("../tests/fixtures/cookies_large.db");

    const YEAR_2100: i64 = 4_102_444_800;
    const YEAR_2101: i64 = 4_133_980_800;
    const NOW: i64 = 1_800_000_000; // 2027

    fn names(bytes: &[u8]) -> Vec<String> {
        let mut names: Vec<_> = parse_cookie_meta(bytes)
            .unwrap()
            .into_iter()
            .map(|c| format!("{}|{}", c.host, c.name))
            .collect();
        names.sort();
        names
    }

    #[test]
    fn reads_every_cookie_of_a_small_database() {
        assert_eq!(
            names(LOGGED_IN_A),
            [
                ".claude.ai|cf_clearance",
                ".claude.ai|lastActiveOrg",
                ".claude.ai|sessionKey",
                ".claude.ai|sessionKeyLC",
                "claude.ai|anthropic-device-id",
            ]
        );
    }

    #[test]
    fn detects_a_login_session_and_its_expiry() {
        let a = find_session(&parse_cookie_meta(LOGGED_IN_A).unwrap());
        assert_eq!(
            a,
            SessionProbe::LoggedIn {
                expires_unix: Some(YEAR_2100)
            }
        );
        assert!(a.is_active(NOW));

        // Versioned cookie name used by newer builds.
        let b = find_session(&parse_cookie_meta(LOGGED_IN_B).unwrap());
        assert_eq!(
            b,
            SessionProbe::LoggedIn {
                expires_unix: Some(YEAR_2101)
            }
        );
    }

    #[test]
    fn anonymous_cookies_are_not_a_session() {
        // Includes a `sessionKey` cookie for another site, which must not count.
        let probe = find_session(&parse_cookie_meta(LOGGED_OUT).unwrap());
        assert_eq!(probe, SessionProbe::LoggedOut);
        assert!(!probe.is_active(NOW));
    }

    #[test]
    fn reports_an_expired_session() {
        let probe = find_session(&parse_cookie_meta(EXPIRED).unwrap());
        assert!(matches!(
            probe,
            SessionProbe::LoggedIn {
                expires_unix: Some(_)
            }
        ));
        assert!(probe.is_expired(NOW));
        assert!(!probe.is_active(NOW));
    }

    #[test]
    fn walks_interior_pages_and_overflow_chains() {
        let cookies = parse_cookie_meta(LARGE).unwrap();
        assert_eq!(cookies.len(), 602);
        for i in [1, 300, 600] {
            let host = format!("host{i}.example.com");
            assert!(
                cookies
                    .iter()
                    .any(|c| c.host == host && c.name == format!("cookie_{i}"))
            );
        }
        // Both of these rows spill onto overflow pages.
        assert!(
            cookies
                .iter()
                .any(|c| c.host == "big.example.com" && c.name == "huge")
        );
        assert_eq!(
            find_session(&cookies),
            SessionProbe::LoggedIn {
                expires_unix: Some(YEAR_2100)
            }
        );
    }

    #[test]
    fn rejects_files_that_are_not_sqlite() {
        assert!(parse_cookie_meta(b"").is_err());
        assert!(parse_cookie_meta(b"definitely not a database").is_err());
        assert!(parse_cookie_meta(&[0u8; 4096]).is_err());
    }

    #[test]
    fn damaged_databases_fail_cleanly() {
        // Truncation at any point must produce an error or a partial list, never a panic.
        for len in (0..LARGE.len()).step_by(1777) {
            let _ = parse_cookie_meta(&LARGE[..len]);
        }
        // Same for corrupted bytes anywhere in the file.
        let mut state: u32 = 0x2545_f491;
        for _ in 0..400 {
            let mut copy = LOGGED_IN_A.to_vec();
            for _ in 0..8 {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let pos = state as usize % copy.len();
                copy[pos] = (state >> 24) as u8;
            }
            let _ = parse_cookie_meta(&copy);
        }
    }

    #[test]
    fn parses_column_names_from_the_schema() {
        let sql = "CREATE TABLE cookies(creation_utc INTEGER NOT NULL,host_key TEXT NOT NULL, \
                   \"name\" TEXT, price DECIMAL(10, 2), UNIQUE (host_key, name))";
        assert_eq!(
            column_names(sql),
            ["creation_utc", "host_key", "name", "price"]
        );
        assert!(column_names("nonsense").is_empty());
    }

    #[test]
    fn decodes_varints_and_signed_integers() {
        assert_eq!(varint(&[0x7f], 0).unwrap(), (127, 1));
        assert_eq!(varint(&[0x81, 0x00], 0).unwrap(), (128, 2));
        assert_eq!(varint(&[0xff; 9], 0).unwrap(), (u64::MAX, 9));
        assert!(varint(&[0x80], 0).is_err());
        assert_eq!(signed_be(&[0xff]), -1);
        assert_eq!(signed_be(&[0x00, 0x80]), 128);
        assert_eq!(signed_be(&[0x80, 0, 0, 0, 0, 0, 0, 0]), i64::MIN);
    }

    #[test]
    fn probes_a_data_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        assert_eq!(probe_session(dir), SessionProbe::LoggedOut);

        std::fs::write(dir.join("Cookies"), LOGGED_OUT).unwrap();
        assert_eq!(probe_session(dir), SessionProbe::LoggedOut);

        // The Windows location wins when both exist.
        std::fs::create_dir_all(dir.join("Network")).unwrap();
        std::fs::write(dir.join("Network").join("Cookies"), LOGGED_IN_A).unwrap();
        assert_eq!(
            probe_session(dir),
            SessionProbe::LoggedIn {
                expires_unix: Some(YEAR_2100)
            }
        );

        std::fs::write(dir.join("Network").join("Cookies"), b"garbage").unwrap();
        assert_eq!(probe_session(dir), SessionProbe::Unknown);
    }
}
