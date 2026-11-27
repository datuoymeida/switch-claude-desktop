//! Finding, quitting and launching the Claude Desktop app.
//!
//! The session files may only be swapped while Claude is not running: the app keeps its
//! cookie store open and writes it back periodically. So a switch is "quit, swap, launch".

use anyhow::{Result, bail};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

/// Bundle identifier of Claude Desktop on macOS.
pub const MACOS_BUNDLE_ID: &str = "com.anthropic.claudefordesktop";

const POLL: Duration = Duration::from_millis(250);
/// How long Claude gets to shut down on its own after being asked to quit.
#[cfg(not(windows))]
const POLITE_TIMEOUT: Duration = Duration::from_secs(20);
/// On Windows the polite request only works if Claude is not set to stay in the tray,
/// so it gets a short wait before the processes are ended.
#[cfg(windows)]
const POLITE_TIMEOUT: Duration = Duration::from_secs(4);
const FORCE_TIMEOUT: Duration = Duration::from_secs(5);
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(8);
/// Pause after the last process is gone so the OS releases its file handles.
const SETTLE: Duration = Duration::from_millis(400);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuitMode {
    /// Ask the app to quit and give it time to save its state.
    Graceful,
    /// End the processes immediately.
    Force,
}

#[derive(Debug)]
pub enum QuitError {
    /// Claude was asked to quit but is still running (for example it is showing a
    /// confirmation dialog). The caller may retry with [`QuitMode::Force`].
    StillRunning,
    Failed(anyhow::Error),
}

/// What the switching logic needs from the Claude Desktop app. A trait so the logic can be
/// tested without touching a real installation.
pub trait ClaudeControl: Send {
    fn is_running(&self) -> bool;
    /// Quit Claude and wait until all of its processes are gone. Succeeds immediately if
    /// it is not running.
    fn quit(&self, mode: QuitMode) -> Result<(), QuitError>;
    fn launch(&self) -> Result<()>;
    /// Whether a graceful quit lets Claude write pending state (recent cookie changes) to
    /// disk. Where it does not, callers wait for a fresh login to reach the disk first.
    fn flushes_on_quit(&self) -> bool;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcKind {
    /// The app itself.
    Main,
    /// A process that only lives as long as the app (renderer, GPU, network service…).
    Helper,
}

/// Classify a macOS executable path.
///
/// The comparison is case-sensitive on purpose: the Claude Code runtime that the app
/// downloads into its data directory is a separate bundle named `claude.app`.
pub fn classify_macos(exe: &str) -> Option<ProcKind> {
    const MARKER: &str = "/Claude.app/Contents/";
    let inside = &exe[exe.find(MARKER)? + MARKER.len()..];
    if inside == "MacOS/Claude" {
        Some(ProcKind::Main)
    } else if inside.starts_with("Frameworks/") {
        // Electron helpers, the crash handler, the updater.
        Some(ProcKind::Helper)
    } else {
        // Other bundled tools (e.g. the browser-extension bridge) run independently.
        None
    }
}

/// Classify a Windows executable path.
///
/// Every Electron process of the app is `Claude.exe`, but so is the unrelated Claude Code
/// command-line tool (`claude.exe`; Windows file names are case-insensitive), which must
/// never be closed. The desktop app is recognised by where it is installed, or by the
/// `resources\app.asar` that sits next to any Electron executable.
pub fn classify_windows(
    exe: &str,
    args: &[String],
    has_app_asar: impl FnOnce() -> bool,
) -> Option<ProcKind> {
    let path = exe.replace('/', "\\").to_ascii_lowercase();
    if path.rsplit('\\').next() != Some("claude.exe") {
        return None;
    }
    // The CLI runtime that the app keeps under "<data dir>\claude-code\".
    if path.contains("\\claude-code\\") {
        return None;
    }
    let known_install = path.contains("\\windowsapps\\claude_")        // MSIX package
        || path.ends_with("\\microsoft\\windowsapps\\claude.exe")      // MSIX launch alias
        || path.contains("\\anthropicclaude\\"); // legacy Squirrel installer
    if !known_install && !has_app_asar() {
        return None;
    }
    // Chromium child processes are started with `--type=renderer` and the like.
    let is_child = args.iter().any(|arg| arg.starts_with("--type="));
    Some(if is_child {
        ProcKind::Helper
    } else {
        ProcKind::Main
    })
}

fn classify(exe: &Path, cmd: &[OsString]) -> Option<ProcKind> {
    #[cfg(windows)]
    {
        let args: Vec<String> = cmd
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        classify_windows(&exe.to_string_lossy(), &args, || {
            exe.parent()
                .is_some_and(|dir| dir.join("resources").join("app.asar").is_file())
        })
    }
    #[cfg(not(windows))]
    {
        let _ = cmd;
        classify_macos(&exe.to_string_lossy())
    }
}

struct Found {
    pid: Pid,
    kind: ProcKind,
    exe: PathBuf,
}

/// List the running processes that belong to Claude Desktop.
fn refresh(system: &mut System) -> Vec<Found> {
    let what = ProcessRefreshKind::nothing().with_exe(UpdateKind::Always);
    // The command line is only needed on Windows, to tell the app from its children.
    #[cfg(windows)]
    let what = what.with_cmd(UpdateKind::Always);
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, what);
    system
        .processes()
        .iter()
        .filter_map(|(pid, process)| {
            let exe = process.exe()?;
            let kind = classify(exe, process.cmd())?;
            Some(Found {
                pid: *pid,
                kind,
                exe: exe.to_path_buf(),
            })
        })
        .collect()
}

/// Ask the app to quit the way the platform allows.
#[cfg(unix)]
fn request_quit(system: &System, found: &[Found]) {
    // Electron turns SIGTERM into a normal `app.quit()`, which saves state on the way out.
    let has_main = found.iter().any(|f| f.kind == ProcKind::Main);
    for f in found
        .iter()
        .filter(|f| !has_main || f.kind == ProcKind::Main)
    {
        if let Some(process) = system.process(f.pid) {
            let _ = process.kill_with(sysinfo::Signal::Term);
        }
    }
}

#[cfg(windows)]
fn request_quit(_system: &System, found: &[Found]) {
    // Without /F, taskkill posts WM_CLOSE to the app's windows.
    for f in found.iter().filter(|f| f.kind == ProcKind::Main) {
        let _ = quiet_command("taskkill")
            .args(["/PID", &f.pid.to_string()])
            .status();
    }
}

#[cfg(not(any(unix, windows)))]
fn request_quit(_system: &System, _found: &[Found]) {}

fn kill_all(system: &System, found: &[Found]) {
    // The app first, so it cannot respawn the helpers.
    for kind in [ProcKind::Main, ProcKind::Helper] {
        for f in found.iter().filter(|f| f.kind == kind) {
            if let Some(process) = system.process(f.pid) {
                let _ = process.kill();
            }
        }
    }
}

fn wait_until_gone(system: &mut System, deadline: Instant) -> bool {
    loop {
        if refresh(system).is_empty() {
            std::thread::sleep(SETTLE);
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POLL);
    }
}

/// A command that never opens a console window or inherits our standard streams.
fn quiet_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut command = Command::new(program);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

/// How to judge a launch command once it has been started.
#[derive(Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // each platform uses its own subset
enum Wait {
    /// The command is a launcher that must exit successfully.
    ForSuccess,
    /// The command is a launcher whose exit code means nothing (`explorer.exe`).
    ForExit,
    /// The command is the app itself; leave it running.
    No,
}

/// The real Claude Desktop app on this machine.
pub struct DesktopClaude {
    /// Claude's data directory; on Windows it reveals which kind of install is present.
    #[cfg_attr(not(windows), allow(dead_code))]
    data_dir: Option<PathBuf>,
    system: Mutex<System>,
    /// Executable of the app when it was last seen running: a launch fallback.
    last_main_exe: Mutex<Option<PathBuf>>,
}

impl DesktopClaude {
    pub fn new(data_dir: Option<PathBuf>) -> Result<Self> {
        // Without a process list there is no telling whether Claude is running, and
        // swapping its files while it runs would corrupt the session. So this is fatal.
        let system = System::new().map_err(|e| {
            anyhow::anyhow!("Không đọc được danh sách tiến trình của hệ thống: {e}")
        })?;
        Ok(Self {
            data_dir,
            system: Mutex::new(system),
            last_main_exe: Mutex::new(None),
        })
    }

    fn remember(&self, found: &[Found]) {
        if let Some(main) = found.iter().find(|f| f.kind == ProcKind::Main) {
            *self
                .last_main_exe
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(main.exe.clone());
        }
    }

    fn wait_until_running(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.is_running() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(POLL);
        }
    }

    /// Ways to start Claude on this platform, best first.
    #[cfg(target_os = "macos")]
    fn launch_steps(&self) -> Vec<(Command, Wait)> {
        let open = |args: [&str; 2]| {
            let mut command = quiet_command("/usr/bin/open");
            command.args(args);
            (command, Wait::ForSuccess)
        };
        vec![open(["-b", MACOS_BUNDLE_ID]), open(["-a", "Claude"])]
    }

    #[cfg(windows)]
    fn launch_steps(&self) -> Vec<(Command, Wait)> {
        let mut steps = Vec::new();

        // MSIX package: start it through its Start-menu identity.
        let family = self
            .data_dir
            .as_deref()
            .and_then(|dir| crate::paths::msix_family_name(&dir.to_string_lossy()));
        if let Some(family) = family {
            let mut command = quiet_command("explorer.exe");
            command.arg(format!("shell:AppsFolder\\{family}!Claude"));
            steps.push((command, Wait::ForExit));
        }

        // Classic install: the executable we saw running, or the installer's launcher.
        let remembered = self
            .last_main_exe
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let mut executables: Vec<PathBuf> = remembered
            .into_iter()
            // Packaged executables cannot be started directly from WindowsApps.
            .filter(|exe| {
                !exe.to_string_lossy()
                    .to_ascii_lowercase()
                    .contains("\\windowsapps\\")
            })
            .collect();
        if let Some(local) = dirs::data_local_dir() {
            executables.push(local.join("AnthropicClaude").join("claude.exe"));
            // Launch alias that the MSIX package registers.
            executables.push(
                local
                    .join("Microsoft")
                    .join("WindowsApps")
                    .join("Claude.exe"),
            );
        }
        for exe in executables {
            if std::fs::symlink_metadata(&exe).is_ok() {
                steps.push((quiet_command(exe), Wait::No));
            }
        }

        // Last resort: the claude:// link handler that every install registers.
        let mut command = quiet_command("cmd");
        command.args(["/C", "start", "", "claude://"]);
        steps.push((command, Wait::ForExit));
        steps
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    fn launch_steps(&self) -> Vec<(Command, Wait)> {
        Vec::new()
    }
}

impl ClaudeControl for DesktopClaude {
    fn is_running(&self) -> bool {
        let mut system = self.system.lock().unwrap_or_else(PoisonError::into_inner);
        let found = refresh(&mut system);
        self.remember(&found);
        found.iter().any(|f| f.kind == ProcKind::Main)
    }

    fn quit(&self, mode: QuitMode) -> Result<(), QuitError> {
        let mut system = self.system.lock().unwrap_or_else(PoisonError::into_inner);
        let found = refresh(&mut system);
        if found.is_empty() {
            return Ok(());
        }
        self.remember(&found);

        let deadline = match mode {
            QuitMode::Graceful => {
                request_quit(&system, &found);
                Instant::now() + POLITE_TIMEOUT
            }
            QuitMode::Force => {
                kill_all(&system, &found);
                Instant::now() + FORCE_TIMEOUT
            }
        };
        if wait_until_gone(&mut system, deadline) {
            return Ok(());
        }

        // Windows gives no way to make an app that hides in the tray quit by itself, so
        // there the polite request is followed by ending the processes.
        if cfg!(windows) && mode == QuitMode::Graceful {
            let left = refresh(&mut system);
            kill_all(&system, &left);
            if wait_until_gone(&mut system, Instant::now() + FORCE_TIMEOUT) {
                return Ok(());
            }
        }
        Err(QuitError::StillRunning)
    }

    fn launch(&self) -> Result<()> {
        if self.is_running() {
            return Ok(());
        }
        for (mut command, wait) in self.launch_steps() {
            let started = match wait {
                Wait::ForSuccess => command.status().is_ok_and(|status| status.success()),
                Wait::ForExit => command.status().is_ok(),
                Wait::No => command.spawn().is_ok(),
            };
            if started && self.wait_until_running(LAUNCH_TIMEOUT) {
                return Ok(());
            }
        }
        bail!("Không tự mở được Claude Desktop. Hãy mở Claude thủ công.")
    }

    fn flushes_on_quit(&self) -> bool {
        // SIGTERM leads to a clean Electron shutdown; ending a process on Windows does not.
        cfg!(unix)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_the_macos_app_and_its_helpers() {
        assert_eq!(
            classify_macos("/Applications/Claude.app/Contents/MacOS/Claude"),
            Some(ProcKind::Main)
        );
        assert_eq!(
            classify_macos("/Users/dat/Applications/Claude.app/Contents/MacOS/Claude"),
            Some(ProcKind::Main)
        );
        assert_eq!(
            classify_macos(
                "/Applications/Claude.app/Contents/Frameworks/Claude Helper (Renderer).app/Contents/MacOS/Claude Helper (Renderer)"
            ),
            Some(ProcKind::Helper)
        );
        assert_eq!(
            classify_macos(
                "/Applications/Claude.app/Contents/Frameworks/Electron Framework.framework/Helpers/chrome_crashpad_handler"
            ),
            Some(ProcKind::Helper)
        );
    }

    #[test]
    fn leaves_other_macos_processes_alone() {
        // The Claude Code runtime inside the data directory: same name, different case.
        assert_eq!(
            classify_macos(
                "/Users/dat/Library/Application Support/Claude/claude-code/2.1.330/4262a1273231/claude.app/Contents/MacOS/claude"
            ),
            None
        );
        // Tools inside the bundle that outlive the app.
        assert_eq!(
            classify_macos("/Applications/Claude.app/Contents/Helpers/chrome-native-host"),
            None
        );
        assert_eq!(classify_macos("/Users/dat/.local/bin/claude"), None);
        // This tool's own bundle ends in "Claude.app" too; it must never quit itself.
        assert_eq!(
            classify_macos("/Applications/AppSwitchClaude.app/Contents/MacOS/AppSwitchClaude"),
            None
        );
        assert_eq!(
            classify_macos("/Applications/Safari.app/Contents/MacOS/Safari"),
            None
        );
        assert_eq!(classify_macos(""), None);
    }

    fn windows(exe: &str, args: &[&str], asar: bool) -> Option<ProcKind> {
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        classify_windows(exe, &args, || asar)
    }

    #[test]
    fn recognises_the_windows_app_in_each_install_flavour() {
        let msix =
            r"C:\Program Files\WindowsApps\Claude_1.8555.2.0_x64__pzs8sxrjxfjjc\app\Claude.exe";
        assert_eq!(windows(msix, &[], false), Some(ProcKind::Main));
        assert_eq!(
            windows(msix, &["--type=renderer", "--user-data-dir=x"], false),
            Some(ProcKind::Helper)
        );
        assert_eq!(
            windows(
                r"C:\Users\dat\AppData\Local\AnthropicClaude\app-0.9.3\claude.exe",
                &[],
                false
            ),
            Some(ProcKind::Main)
        );
        assert_eq!(
            windows(
                r"C:\Users\dat\AppData\Local\Microsoft\WindowsApps\Claude.exe",
                &[],
                false
            ),
            Some(ProcKind::Main)
        );
        // An install somewhere unexpected is still an Electron app with its app.asar.
        assert_eq!(
            windows(r"D:\Apps\Claude\Claude.exe", &[], true),
            Some(ProcKind::Main)
        );
        assert_eq!(
            windows(r"D:\Apps\Claude\Claude.exe", &["--type=gpu-process"], true),
            Some(ProcKind::Helper)
        );
    }

    #[test]
    fn never_matches_the_claude_code_cli_on_windows() {
        assert_eq!(
            windows(r"C:\Users\dat\.local\bin\claude.exe", &[], false),
            None
        );
        assert_eq!(
            windows(
                r"C:\Users\dat\AppData\Roaming\npm\node_modules\@anthropic-ai\claude-code\claude.exe",
                &[],
                false
            ),
            None
        );
        // The runtime kept inside the desktop app's own data directory.
        assert_eq!(
            windows(
                r"C:\Users\dat\AppData\Local\Packages\Claude_pzs8sxrjxfjjc\LocalCache\Roaming\Claude\claude-code\2.1.330\claude.exe",
                &[],
                true
            ),
            None
        );
        assert_eq!(windows(r"C:\Windows\explorer.exe", &[], true), None);
        // This tool itself.
        assert_eq!(windows(r"C:\Tools\app-switch-claude.exe", &[], false), None);
        assert_eq!(windows(r"C:\Tools\notclaude.exe", &[], true), None);
    }
}
