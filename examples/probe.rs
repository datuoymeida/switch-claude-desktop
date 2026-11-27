//! Read-only diagnostic: prints what this tool detects about Claude Desktop on this
//! machine. Nothing is modified and no secret value (cookie, token) is ever printed.
//!
//!     cargo run --example probe
//!
//! Useful when a switch misbehaves, in particular on a Windows install this tool has not
//! seen before: the output shows which data directory, cookie store and processes it found.

use app_switch_claude::claude_app::{ClaudeControl, DesktopClaude};
use app_switch_claude::{config_json, cookies, paths, session};

fn main() -> anyhow::Result<()> {
    println!("Candidate data directories:");
    for candidate in paths::candidates() {
        let mark = if candidate.is_dir() { "x" } else { " " };
        println!("  [{mark}] {}", candidate.display());
    }
    let Some(dir) = paths::claude_data_dir() else {
        println!("No Claude Desktop data directory found.");
        return Ok(());
    };
    println!("Using: {}", dir.display());
    if let Some(family) = paths::msix_family_name(&dir.to_string_lossy()) {
        println!("MSIX package family: {family}");
    }

    println!("\nSession entries:");
    for item in session::SESSION_ITEMS {
        let path = item.split('/').fold(dir.clone(), |p, part| p.join(part));
        if path.exists() {
            println!("  present  {item}");
        }
    }

    println!("\nCookie store:");
    match cookies::cookie_db_path(&dir) {
        None => println!("  not found"),
        Some(db) => match cookies::read_cookie_meta(&db) {
            Err(error) => println!("  {} is unreadable: {error:#}", db.display()),
            Ok(list) => {
                println!("  {} ({} cookies)", db.display(), list.len());
                for cookie in list.iter().filter(|c| c.host.ends_with("claude.ai")) {
                    let expiry = cookie
                        .expires_unix
                        .map(|t| format!("expires at unix time {t}"))
                        .unwrap_or_else(|| "no expiry".to_owned());
                    println!("    {:<12} {:<32} {expiry}", cookie.host, cookie.name);
                }
            }
        },
    }
    println!("  login session: {:?}", cookies::probe_session(&dir));

    println!("\nconfig.json:");
    match config_json::read_effective(&dir) {
        Err(error) => println!("  unreadable: {error:#}"),
        Ok(None) => println!("  not found"),
        Ok(Some(config)) => {
            let journal = dir.join(config_json::JOURNAL_FILE).exists();
            println!("  {} keys (journal file present: {journal})", config.len());
            let keys = config_json::account_keys(&config);
            let names: Vec<&str> = keys.keys().map(String::as_str).collect();
            println!("  per-account keys: {names:?}");
            let id = config_json::account_uuid(&config)
                .map(|uuid| format!("{}…", uuid.chars().take(8).collect::<String>()))
                .unwrap_or_else(|| "none recorded".to_owned());
            println!("  account id: {id}");
        }
    }

    let claude = DesktopClaude::new(Some(dir))?;
    println!("\nClaude Desktop running: {}", claude.is_running());
    println!(
        "Saved accounts are kept in: {}",
        paths::store_dir()?.display()
    );
    Ok(())
}
