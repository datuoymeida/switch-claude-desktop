//! The window: the account Claude is using, the saved accounts, and one-click switching.
//!
//! [`View`] only draws and reports what the user asked for as a [`Job`]; it never touches
//! Claude or the disk. [`app`] runs the jobs on a background thread.

pub mod app;
pub mod style;

use eframe::egui::{
    self, Align, Button, Frame, Label, Layout, Margin, RichText, Sides, Stroke, TextEdit, Ui,
};

use crate::cookies::SessionProbe;
use crate::fsutil::now_unix;
use crate::store::MAX_LABEL_CHARS;
use crate::switcher::{AccountView, LiveSession, Overview};
use style::{Palette, bold};

pub const WINDOW_TITLE: &str = "Chuyển tài khoản Claude";

/// Something the user asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Job {
    Refresh,
    SaveCurrent {
        label: String,
        force_quit: bool,
        allow_unverified: bool,
    },
    AddAccount {
        force_quit: bool,
    },
    Switch {
        id: String,
        force_quit: bool,
    },
    Remove {
        id: String,
    },
    Rename {
        id: String,
        label: String,
    },
    OpenClaude,
}

impl Job {
    /// The same job, allowed to end Claude's processes.
    fn forcing_quit(mut self) -> Self {
        match &mut self {
            Job::SaveCurrent { force_quit, .. }
            | Job::AddAccount { force_quit }
            | Job::Switch { force_quit, .. } => *force_quit = true,
            Job::Refresh | Job::Remove { .. } | Job::Rename { .. } | Job::OpenClaude => {}
        }
        self
    }

    /// The same job, allowed to save without a detected login.
    fn allowing_unverified(mut self) -> Self {
        if let Job::SaveCurrent {
            allow_unverified, ..
        } = &mut self
        {
            *allow_unverified = true;
        }
        self
    }
}

/// How a job ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Done(String),
    Failed(String),
    /// Claude would not quit. The user decides whether to end its processes.
    ClaudeStillRunning(Job),
    /// No login was found in Claude. The user decides whether to save anyway.
    NotLoggedIn(Job),
}

struct Notice {
    text: String,
    is_error: bool,
}

enum Dialog {
    Add,
    Remove {
        id: String,
        label: String,
    },
    Rename {
        id: String,
        text: String,
        focused: bool,
    },
    ForceQuit {
        job: Job,
    },
    NotLoggedIn {
        job: Job,
    },
}

impl Dialog {
    /// The job to run when the user confirms this dialog.
    fn confirmed_job(&self) -> Job {
        match self {
            Dialog::Add => Job::AddAccount { force_quit: false },
            Dialog::Remove { id, .. } => Job::Remove { id: id.clone() },
            Dialog::Rename { id, text, .. } => Job::Rename {
                id: id.clone(),
                label: text.clone(),
            },
            Dialog::ForceQuit { job } => job.clone().forcing_quit(),
            Dialog::NotLoggedIn { job } => job.clone().allowing_unverified(),
        }
    }
}

#[derive(Default)]
pub struct View {
    overview: Option<Overview>,
    load_error: Option<String>,
    /// Status line of the job in progress.
    busy: Option<String>,
    notice: Option<Notice>,
    /// Name being typed for the account Claude is signed in to.
    new_label: String,
    dialog: Option<Dialog>,
}

impl View {
    pub fn is_busy(&self) -> bool {
        self.busy.is_some()
    }

    pub fn set_overview(&mut self, overview: Result<Overview, String>) {
        match overview {
            Ok(overview) => {
                self.overview = Some(overview);
                self.load_error = None;
            }
            Err(error) => self.load_error = Some(error),
        }
    }

    pub fn job_started(&mut self) {
        self.busy = Some(String::from("Đang xử lý…"));
        self.notice = None;
    }

    pub fn set_progress(&mut self, text: String) {
        if self.busy.is_some() {
            self.busy = Some(text);
        }
    }

    pub fn job_finished(&mut self, outcome: Outcome) {
        self.busy = None;
        match outcome {
            Outcome::Done(text) => {
                self.new_label.clear();
                self.notice = Some(Notice {
                    text,
                    is_error: false,
                });
            }
            Outcome::Failed(text) => {
                self.notice = Some(Notice {
                    text,
                    is_error: true,
                })
            }
            Outcome::ClaudeStillRunning(job) => self.dialog = Some(Dialog::ForceQuit { job }),
            Outcome::NotLoggedIn(job) => self.dialog = Some(Dialog::NotLoggedIn { job }),
        }
    }

    /// Draw the window. Returns the job the user just asked for, if any.
    pub fn show(&mut self, ui: &mut Ui) -> Option<Job> {
        let palette = Palette::of(ui.visuals().dark_mode);
        let mut job = None;
        let Self {
            overview,
            load_error,
            busy,
            notice,
            new_label,
            dialog,
        } = self;

        let bar = Frame::new()
            .fill(palette.background)
            .inner_margin(Margin::symmetric(16, 10));
        egui::Panel::bottom("status")
            .frame(bar)
            .show_separator_line(false)
            .show(ui, |ui| {
                status_bar(ui, &palette, busy.as_deref(), notice);
            });

        let body = Frame::new().fill(palette.background).inner_margin(Margin {
            left: 16,
            right: 16,
            top: 14,
            bottom: 0,
        });
        egui::CentralPanel::default().frame(body).show(ui, |ui| {
            header(ui, &palette, overview.as_ref());
            if let Some(error) = load_error {
                ui.colored_label(palette.bad, error.as_str());
            }
            let Some(overview) = overview.as_ref() else {
                if load_error.is_none() {
                    ui.label(RichText::new("Đang kiểm tra Claude Desktop…").color(palette.muted));
                }
                return;
            };
            ui.add_enabled_ui(busy.is_none(), |ui| {
                live_card(ui, &palette, overview, new_label, &mut job);
                ui.add_space(6.0);
                account_list(ui, &palette, overview, &mut job, dialog);
            });
        });

        show_dialog(ui.ctx(), &palette, dialog, &mut job);
        job
    }
}

fn header(ui: &mut Ui, palette: &Palette, overview: Option<&Overview>) {
    Sides::new().show(
        ui,
        |ui| {
            ui.label(RichText::new(WINDOW_TITLE).heading());
        },
        |ui| {
            if let Some(overview) = overview {
                let (text, color) = if overview.claude_running {
                    ("Claude đang chạy", palette.good)
                } else {
                    ("Claude đang tắt", palette.muted)
                };
                ui.label(RichText::new(text).small().color(color));
                dot(ui, color);
            }
        },
    );
}

/// The account Claude is signed in to right now.
fn live_card(
    ui: &mut Ui,
    palette: &Palette,
    overview: &Overview,
    new_label: &mut String,
    job: &mut Option<Job>,
) {
    card(ui, palette, |ui| match &overview.live {
        LiveSession::Saved { id } => {
            caption(ui, palette.muted, "ĐANG DÙNG TRONG CLAUDE");
            if let Some(view) = overview.accounts.iter().find(|a| &a.account.id == id) {
                ui.add(
                    Label::new(RichText::new(&view.account.label).family(bold()).size(18.0))
                        .truncate(),
                );
                session_line(ui, palette, view);
            }
        }
        LiveSession::Unsaved { .. } => {
            caption(ui, palette.accent, "TÀI KHOẢN CHƯA LƯU");
            let prompt = ui.label(
                "Claude đang đăng nhập một tài khoản chưa có trong danh sách. \
                 Đặt tên để lưu lại:",
            );
            ui.horizontal(|ui| {
                let button_width = 132.0;
                let field_width =
                    (ui.available_width() - button_width - ui.spacing().item_spacing.x).max(80.0);
                let field = ui
                    .add(
                        TextEdit::singleline(new_label)
                            .hint_text("Ví dụ: Công việc")
                            .char_limit(MAX_LABEL_CHARS)
                            .desired_width(field_width),
                    )
                    .labelled_by(prompt.id);
                let entered =
                    field.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
                let ready = !new_label.trim().is_empty();
                let clicked = ui
                    .add_enabled(ready, primary_button(palette, "Lưu tài khoản này"))
                    .clicked();
                if ready && (clicked || entered) {
                    *job = Some(Job::SaveCurrent {
                        label: new_label.clone(),
                        force_quit: false,
                        allow_unverified: false,
                    });
                }
            });
        }
        LiveSession::LoggedOut => {
            caption(ui, palette.muted, "CLAUDE CHƯA ĐĂNG NHẬP");
            ui.label("Đăng nhập trong Claude Desktop, tài khoản sẽ hiện ở đây để bạn lưu.");
            if !overview.claude_running && ui.button("Mở Claude").clicked() {
                *job = Some(Job::OpenClaude);
            }
        }
        LiveSession::Interrupted => {
            caption(ui, palette.warn, "LẦN ĐỔI TÀI KHOẢN TRƯỚC CHƯA XONG");
            ui.label("Hãy chọn một tài khoản bên dưới và bấm Chuyển để hoàn tất.");
        }
        LiveSession::NoDataDir => {
            caption(ui, palette.bad, "KHÔNG TÌM THẤY CLAUDE DESKTOP");
            ui.label("Hãy cài Claude Desktop và mở nó ít nhất một lần, rồi mở lại ứng dụng này.");
            if let Some(dir) = &overview.data_dir {
                ui.label(
                    RichText::new(format!("Đã tìm ở: {}", dir.display()))
                        .small()
                        .color(palette.muted),
                );
            }
        }
    });
}

fn account_list(
    ui: &mut Ui,
    palette: &Palette,
    overview: &Overview,
    job: &mut Option<Job>,
    dialog: &mut Option<Dialog>,
) {
    let can_add = overview.live != LiveSession::NoDataDir;
    Sides::new().show(
        ui,
        |ui| {
            ui.label(RichText::new("Tài khoản đã lưu").family(bold()).size(15.0));
            ui.label(RichText::new(overview.accounts.len().to_string()).color(palette.muted));
        },
        |ui| {
            let add = ui
                .add_enabled(can_add, Button::new("+  Thêm tài khoản"))
                .on_hover_text("Mở Claude ở màn hình đăng nhập để đăng nhập một tài khoản khác");
            if add.clicked() {
                *dialog = Some(Dialog::Add);
            }
        },
    );

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            if overview.accounts.is_empty() {
                ui.add_space(24.0);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("Chưa có tài khoản nào được lưu.").color(palette.muted));
                });
            }
            for view in &overview.accounts {
                account_row(ui, palette, view, job, dialog);
            }
            ui.add_space(4.0);
        });
}

fn account_row(
    ui: &mut Ui,
    palette: &Palette,
    view: &AccountView,
    job: &mut Option<Job>,
    dialog: &mut Option<Dialog>,
) {
    let account = &view.account;
    card(ui, palette, |ui| {
        Sides::new().shrink_left().show(
            ui,
            |ui| {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 3.0;
                    ui.horizontal(|ui| {
                        ui.add(
                            Label::new(RichText::new(&account.label).family(bold()).size(15.0))
                                .truncate(),
                        );
                        if view.is_live {
                            badge(ui, palette, "Đang dùng");
                        }
                    });
                    session_line(ui, palette, view);
                });
            },
            |ui| {
                // Laid out right to left.
                if ui.button("Xoá").clicked() {
                    *dialog = Some(Dialog::Remove {
                        id: account.id.clone(),
                        label: account.label.clone(),
                    });
                }
                if ui.button("Đổi tên").clicked() {
                    *dialog = Some(Dialog::Rename {
                        id: account.id.clone(),
                        text: account.label.clone(),
                        focused: false,
                    });
                }
                if !view.is_live {
                    let switch =
                        ui.add_enabled(view.has_snapshot, primary_button(palette, "Chuyển"));
                    if switch.clicked() {
                        *job = Some(Job::Switch {
                            id: account.id.clone(),
                            force_quit: false,
                        });
                    }
                }
            },
        );
    });
}

/// One line about the state of an account's saved login.
fn session_line(ui: &mut Ui, palette: &Palette, view: &AccountView) {
    let (text, needs_attention) = session_text(view, now_unix());
    let color = if needs_attention {
        palette.warn
    } else {
        palette.muted
    };
    ui.add(Label::new(RichText::new(text).small().color(color)).truncate());
}

fn session_text(view: &AccountView, now: i64) -> (String, bool) {
    const DAY: i64 = 86_400;
    if !view.has_snapshot && !view.is_live {
        return (String::from("Chưa có phiên đăng nhập được lưu"), true);
    }
    match view.session {
        SessionProbe::LoggedIn {
            expires_unix: Some(expires),
        } if expires <= now => (String::from("Phiên đã hết hạn — cần đăng nhập lại"), true),
        SessionProbe::LoggedIn {
            expires_unix: Some(expires),
        } if expires - now < 3 * DAY => (format!("Phiên sắp hết hạn ({})", date(expires)), true),
        SessionProbe::LoggedIn {
            expires_unix: Some(expires),
        } => (format!("Phiên còn hạn đến {}", date(expires)), false),
        SessionProbe::LoggedIn { expires_unix: None } => {
            (String::from("Đã lưu phiên đăng nhập"), false)
        }
        SessionProbe::LoggedOut => (String::from("Bản lưu không có phiên đăng nhập"), true),
        SessionProbe::Unknown => (String::from("Không đọc được trạng thái phiên"), false),
    }
}

/// A Unix time as a local calendar date.
fn date(unix: i64) -> String {
    chrono::DateTime::from_timestamp(unix, 0)
        .map(|utc| {
            utc.with_timezone(&chrono::Local)
                .format("%d/%m/%Y")
                .to_string()
        })
        .unwrap_or_else(|| String::from("?"))
}

fn status_bar(ui: &mut Ui, palette: &Palette, busy: Option<&str>, notice: &mut Option<Notice>) {
    ui.spacing_mut().item_spacing.y = 6.0;
    if let Some(progress) = busy {
        ui.horizontal(|ui| {
            ui.add(egui::Spinner::new().size(16.0));
            ui.label(progress);
        });
    } else if let Some(current) = notice {
        let color = if current.is_error {
            palette.bad
        } else {
            palette.good
        };
        let mut dismissed = false;
        Sides::new()
            .shrink_left()
            .wrap_mode(egui::TextWrapMode::Wrap)
            .show(
                ui,
                |ui| {
                    ui.add(Label::new(RichText::new(&current.text).color(color)).wrap());
                },
                |ui| {
                    dismissed = ui.add(Button::new("Đóng").small()).clicked();
                },
            );
        if dismissed {
            *notice = None;
        }
    }
    ui.add(
        Label::new(
            RichText::new(
                "Lưu ý: đừng bấm “Log out” trong Claude, vì thao tác đó huỷ phiên đã lưu. \
                 Muốn đăng nhập tài khoản khác, hãy dùng “Thêm tài khoản”.",
            )
            .small()
            .color(palette.muted),
        )
        .wrap(),
    );
}

fn show_dialog(
    ctx: &egui::Context,
    palette: &Palette,
    dialog: &mut Option<Dialog>,
    job: &mut Option<Job>,
) {
    let Some(current) = dialog.as_mut() else {
        return;
    };
    let frame = Frame::window(&ctx.global_style()).inner_margin(Margin::same(18));
    let modal = egui::Modal::new(egui::Id::unique("dialog")).frame(frame).show(ctx, |ui| {
        ui.set_width(350.0);
        match current {
            Dialog::Add => {
                dialog_text(
                    ui,
                    "Thêm tài khoản khác",
                    "Claude sẽ khởi động lại ở màn hình đăng nhập. Tài khoản đang dùng được lưu \
                     lại trước và không bị đăng xuất khỏi máy chủ.\n\nSau khi đăng nhập tài khoản \
                     mới trong Claude, quay lại đây để đặt tên và lưu.",
                );
                dialog_buttons(ui, true, primary_button(palette, "Tiếp tục"))
            }
            Dialog::Remove { label, .. } => {
                dialog_text(
                    ui,
                    &format!("Xoá “{label}”?"),
                    "Phiên đã lưu của tài khoản này sẽ bị xoá khỏi máy. Tài khoản Claude và \
                     phiên đang mở trong Claude không bị ảnh hưởng.",
                );
                let delete =
                    Button::new(RichText::new("Xoá").color(palette.on_accent)).fill(palette.bad);
                dialog_buttons(ui, true, delete)
            }
            Dialog::Rename { text, focused, .. } => {
                let title =
                    ui.label(RichText::new("Đổi tên tài khoản").family(bold()).size(16.0));
                let field = ui
                    .add(
                        TextEdit::singleline(text)
                            .char_limit(MAX_LABEL_CHARS)
                            .desired_width(f32::INFINITY),
                    )
                    .labelled_by(title.id);
                if !*focused {
                    field.request_focus();
                    *focused = true;
                }
                let entered =
                    field.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
                let ready = !text.trim().is_empty();
                let choice = dialog_buttons(ui, ready, primary_button(palette, "Lưu"));
                if ready && entered { Choice::Confirm } else { choice }
            }
            Dialog::ForceQuit { .. } => {
                dialog_text(
                    ui,
                    "Claude chưa thoát",
                    "Claude không tự thoát — có thể nó đang hiện một hộp thoại hoặc đang bận. \
                     Bạn có thể tự thoát Claude rồi thử lại, hoặc buộc thoát ngay (việc đang \
                     làm dở trong Claude có thể bị mất).",
                );
                dialog_buttons(ui, true, primary_button(palette, "Buộc thoát và tiếp tục"))
            }
            Dialog::NotLoggedIn { .. } => {
                dialog_text(
                    ui,
                    "Chưa thấy phiên đăng nhập",
                    "Không tìm thấy phiên đăng nhập nào trong Claude. Hãy đăng nhập trong Claude \
                     rồi lưu lại.\n\nNếu chắc chắn Claude đã đăng nhập, bạn vẫn có thể lưu trạng \
                     thái hiện tại.",
                );
                dialog_buttons(ui, true, Button::new("Vẫn lưu"))
            }
        }
    });

    let choice = modal.inner;
    if choice == Choice::Confirm {
        *job = Some(current.confirmed_job());
    }
    if choice != Choice::None || modal.should_close() {
        *dialog = None;
    }
}

fn dialog_text(ui: &mut Ui, title: &str, body: &str) {
    ui.label(RichText::new(title).family(bold()).size(16.0));
    ui.add(Label::new(body).wrap());
}

/// What the user picked in a dialog's button row this frame.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Choice {
    None,
    Confirm,
    Cancel,
}

/// The button row of a dialog: "Huỷ" and `confirm`.
fn dialog_buttons(ui: &mut Ui, confirm_enabled: bool, confirm: Button<'_>) -> Choice {
    ui.add_space(4.0);
    let mut choice = Choice::None;
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        if ui.add_enabled(confirm_enabled, confirm).clicked() {
            choice = Choice::Confirm;
        }
        if ui.button("Huỷ").clicked() {
            choice = Choice::Cancel;
        }
    });
    choice
}

fn card(ui: &mut Ui, palette: &Palette, add_contents: impl FnOnce(&mut Ui)) {
    Frame::new()
        .fill(palette.card)
        .stroke(Stroke::new(1.0, palette.card_stroke))
        .corner_radius(10)
        .inner_margin(Margin::symmetric(14, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add_contents(ui);
        });
}

fn caption(ui: &mut Ui, color: egui::Color32, text: &str) {
    ui.label(RichText::new(text).family(bold()).size(11.0).color(color));
}

fn badge(ui: &mut Ui, palette: &Palette, text: &str) {
    Frame::new()
        .fill(palette.good_bg)
        .corner_radius(99)
        .inner_margin(Margin::symmetric(8, 1))
        .show(ui, |ui| {
            ui.label(RichText::new(text).small().color(palette.good));
        });
}

fn dot(ui: &mut Ui, color: egui::Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.0, color);
}

fn primary_button<'a>(palette: &Palette, text: &'a str) -> Button<'a> {
    Button::new(RichText::new(text).color(palette.on_accent)).fill(palette.accent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Account;

    fn view(session: SessionProbe, has_snapshot: bool, is_live: bool) -> AccountView {
        AccountView {
            account: Account {
                id: "acc-1".into(),
                label: "A".into(),
                account_uuid: None,
                created_at: 0,
                saved_at: 0,
                last_used_at: None,
            },
            is_live,
            has_snapshot,
            session,
        }
    }

    const NOW: i64 = 1_800_000_000;
    const DAY: i64 = 86_400;

    #[test]
    fn describes_the_saved_session() {
        let logged_in = |expires| SessionProbe::LoggedIn {
            expires_unix: expires,
        };

        let (text, attention) =
            session_text(&view(logged_in(Some(NOW + 30 * DAY)), true, false), NOW);
        assert!(text.starts_with("Phiên còn hạn đến "), "{text}");
        assert!(!attention);

        let (text, attention) = session_text(&view(logged_in(Some(NOW + DAY)), true, false), NOW);
        assert!(text.starts_with("Phiên sắp hết hạn ("), "{text}");
        assert!(attention);

        let (text, attention) = session_text(&view(logged_in(Some(NOW - 1)), true, false), NOW);
        assert!(text.starts_with("Phiên đã hết hạn"), "{text}");
        assert!(attention);

        assert_eq!(
            session_text(&view(logged_in(None), true, false), NOW),
            (String::from("Đã lưu phiên đăng nhập"), false)
        );
        assert!(session_text(&view(SessionProbe::LoggedOut, true, false), NOW).1);
        assert!(session_text(&view(SessionProbe::LoggedOut, false, false), NOW).1);
    }

    #[test]
    fn retried_jobs_carry_the_users_consent() {
        let switch = Job::Switch {
            id: "a".into(),
            force_quit: false,
        };
        assert_eq!(
            switch.forcing_quit(),
            Job::Switch {
                id: "a".into(),
                force_quit: true
            }
        );
        assert_eq!(
            Job::AddAccount { force_quit: false }.forcing_quit(),
            Job::AddAccount { force_quit: true }
        );
        let save = Job::SaveCurrent {
            label: "x".into(),
            force_quit: false,
            allow_unverified: false,
        };
        assert_eq!(
            save.clone().forcing_quit(),
            Job::SaveCurrent {
                label: "x".into(),
                force_quit: true,
                allow_unverified: false
            }
        );
        assert_eq!(
            save.allowing_unverified(),
            Job::SaveCurrent {
                label: "x".into(),
                force_quit: false,
                allow_unverified: true
            }
        );
        // Jobs that never quit Claude are unchanged.
        assert_eq!(
            Job::Remove { id: "a".into() }.forcing_quit(),
            Job::Remove { id: "a".into() }
        );
    }

    #[test]
    fn formats_dates_day_first() {
        let text = date(NOW);
        let parts: Vec<&str> = text.split('/').collect();
        assert_eq!(parts.len(), 3, "{text}");
        assert_eq!(parts[2], "2027");
    }
}
