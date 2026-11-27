//! Switch between Claude accounts in the Claude Desktop app without logging out.
//!
//! - [`switcher`] holds the operations (save, switch, add, rename, remove).
//! - [`session`], [`config_json`] and [`cookies`] know how Claude Desktop stores a login.
//! - [`claude_app`] quits and launches the app; [`paths`] finds its data directory.
//! - [`store`] is this tool's own list of saved accounts.
//! - [`ui`] is the window.

pub mod claude_app;
pub mod config_json;
pub mod cookies;
pub mod fsutil;
pub mod paths;
pub mod session;
pub mod store;
pub mod switcher;
pub mod ui;
