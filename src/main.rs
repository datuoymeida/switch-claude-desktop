// A release build on Windows is a GUI program: no console window behind it.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use app_switch_claude::ui::{WINDOW_TITLE, app::App};
use eframe::egui;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(WINDOW_TITLE)
            .with_inner_size([500.0, 640.0])
            .with_min_inner_size([440.0, 420.0]),
        ..Default::default()
    };
    let result = eframe::run_native(
        "AppSwitchClaude",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    );
    // Without a console, a start-up failure (no usable graphics driver, say) would end the
    // program without a trace: say why in a dialog instead.
    #[cfg(windows)]
    if let Err(error) = &result {
        error_box(&format!("Không mở được cửa sổ ứng dụng.\n\n{error}"));
    }
    result
}

/// A native error dialog.
#[cfg(windows)]
fn error_box(text: &str) {
    #[link(name = "user32")]
    unsafe extern "system" {
        fn MessageBoxW(
            owner: *mut std::ffi::c_void,
            text: *const u16,
            caption: *const u16,
            kind: u32,
        ) -> i32;
    }
    const MB_ICONERROR: u32 = 0x10;

    let wide = |text: &str| -> Vec<u16> { text.encode_utf16().chain([0]).collect() };
    let (text, caption) = (wide(text), wide(WINDOW_TITLE));
    // SAFETY: both strings are NUL-terminated UTF-16 buffers that outlive the call, and
    // the dialog needs no owner window.
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            caption.as_ptr(),
            MB_ICONERROR,
        );
    }
}
