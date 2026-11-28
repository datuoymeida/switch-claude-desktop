//! Drives the window headlessly: clicks and typing must turn into the right jobs.
//!
//! `renders_previews` additionally paints each screen to
//! `target/tmp/ui-preview/*.png` when a GPU is available, so the layout can be reviewed
//! without launching the app (and without a real Claude installation).

use eframe::egui::{self, Theme, accesskit::Role};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use std::path::PathBuf;

use app_switch_claude::cookies::SessionProbe;
use app_switch_claude::fsutil::now_unix;
use app_switch_claude::store::Account;
use app_switch_claude::switcher::{AccountView, LiveSession, Overview};
use app_switch_claude::ui::{Job, Outcome, View, style};

const DAY: i64 = 86_400;

struct Screen {
    view: View,
    jobs: Vec<Job>,
}

fn account(id: &str, label: &str, is_live: bool, session: SessionProbe) -> AccountView {
    AccountView {
        account: Account {
            id: id.to_owned(),
            label: label.to_owned(),
            account_uuid: Some(format!("uuid-{id}")),
            created_at: now_unix() - 40 * DAY,
            saved_at: now_unix() - DAY,
            last_used_at: None,
            signed_out: false,
        },
        is_live,
        has_snapshot: true,
        session,
    }
}

fn logged_in(days_left: i64) -> SessionProbe {
    SessionProbe::LoggedIn {
        expires_unix: Some(now_unix() + days_left * DAY),
    }
}

fn overview(live: LiveSession, claude_running: bool, accounts: Vec<AccountView>) -> Overview {
    Overview {
        data_dir: Some(PathBuf::from(
            "/Users/me/Library/Application Support/Claude",
        )),
        store_dir: PathBuf::from("/Users/me/Library/Application Support/AppSwitchClaude"),
        claude_running,
        live,
        accounts,
    }
}

/// Two saved accounts, the first of which Claude is using.
fn two_accounts() -> Overview {
    overview(
        LiveSession::Saved { id: "work".into() },
        true,
        vec![
            account("work", "Công việc", true, logged_in(27)),
            account("home", "Cá nhân", false, logged_in(12)),
        ],
    )
}

fn screen(overview: Overview) -> Screen {
    let mut view = View::default();
    view.set_overview(Ok(overview));
    Screen {
        view,
        jobs: Vec::new(),
    }
}

fn harness(screen: Screen, theme: Theme) -> Harness<'static, Screen> {
    Harness::builder()
        .with_size(egui::vec2(500.0, 640.0))
        .with_pixels_per_point(2.0)
        .with_theme(theme)
        .with_context_setup(style::apply)
        .build_ui_state(
            |ui, screen: &mut Screen| {
                if let Some(job) = screen.view.show(ui) {
                    screen.view.job_started();
                    screen.jobs.push(job);
                }
            },
            screen,
        )
}

/// Run a few frames. (`Harness::run` waits for the UI to stop repainting, which never
/// happens while the progress spinner is on screen.)
fn settle(harness: &mut Harness<'_, Screen>) {
    harness.run_steps(4);
}

fn open(overview: Overview) -> Harness<'static, Screen> {
    let mut harness = harness(screen(overview), Theme::Light);
    settle(&mut harness);
    harness
}

#[test]
fn clicking_switch_asks_for_that_account() {
    let mut harness = open(two_accounts());
    // The account in use has no switch button, so there is exactly one.
    harness.get_by_label("Chuyển").click();
    settle(&mut harness);
    assert_eq!(
        harness.state().jobs,
        [Job::Switch {
            id: "home".into(),
            force_quit: false
        }]
    );
    // While the job runs the window shows progress.
    assert!(harness.state().view.is_busy());
    harness.get_by_label("Đang xử lý…");
}

#[test]
fn naming_an_unsaved_session_saves_it() {
    let live = LiveSession::Unsaved {
        account_uuid: Some("uuid-new".into()),
    };
    let mut harness = open(overview(
        live,
        true,
        vec![account("work", "Công việc", false, logged_in(20))],
    ));

    // Nothing to save until a name is typed.
    harness.get_by_label("Lưu tài khoản này").click();
    settle(&mut harness);
    assert!(harness.state().jobs.is_empty());

    let field = harness.get_by_role(Role::TextInput);
    field.focus();
    field.type_text("Dự án mới");
    settle(&mut harness);
    harness.get_by_label("Lưu tài khoản này").click();
    settle(&mut harness);
    assert_eq!(
        harness.state().jobs,
        [Job::SaveCurrent {
            label: "Dự án mới".into(),
            force_quit: false,
            allow_unverified: false
        }]
    );
}

#[test]
fn removing_needs_confirmation() {
    let mut harness = open(two_accounts());
    let row_buttons = harness.get_all_by_label("Xoá").count();
    assert_eq!(row_buttons, 2);

    // Open the dialog for the second account, then back out.
    harness.get_all_by_label("Xoá").last().unwrap().click();
    settle(&mut harness);
    harness.get_by_label("Xoá “Cá nhân”?");
    harness.get_by_label("Huỷ").click();
    settle(&mut harness);
    assert!(harness.state().jobs.is_empty());
    assert!(harness.query_by_label("Xoá “Cá nhân”?").is_none());

    // Open it again and confirm with the dialog's own button (drawn last).
    harness.get_all_by_label("Xoá").last().unwrap().click();
    settle(&mut harness);
    assert_eq!(harness.get_all_by_label("Xoá").count(), row_buttons + 1);
    harness.get_all_by_label("Xoá").last().unwrap().click();
    settle(&mut harness);
    assert_eq!(harness.state().jobs, [Job::Remove { id: "home".into() }]);
}

#[test]
fn adding_an_account_explains_what_happens_first() {
    let mut harness = open(two_accounts());
    harness.get_by_label("+  Thêm tài khoản").click();
    settle(&mut harness);
    assert!(harness.state().jobs.is_empty());
    harness.get_by_label("Thêm tài khoản khác");
    harness.get_by_label("Tiếp tục").click();
    settle(&mut harness);
    assert_eq!(
        harness.state().jobs,
        [Job::AddAccount { force_quit: false }]
    );
}

#[test]
fn renaming_sends_the_new_name() {
    let mut harness = open(two_accounts());
    harness.get_all_by_label("Đổi tên").next().unwrap().click();
    settle(&mut harness);
    let field = harness.get_by_role(Role::TextInput);
    assert_eq!(field.value().as_deref(), Some("Công việc"));
    field.type_text(" 2");
    settle(&mut harness);
    harness.get_by_label("Lưu").click();
    settle(&mut harness);
    assert_eq!(
        harness.state().jobs,
        [Job::Rename {
            id: "work".into(),
            label: "Công việc 2".into()
        }]
    );
}

#[test]
fn a_stuck_claude_is_only_force_quit_with_consent() {
    let mut harness = open(two_accounts());
    let pending = Job::Switch {
        id: "home".into(),
        force_quit: false,
    };
    harness.state_mut().view.job_started();
    harness
        .state_mut()
        .view
        .job_finished(Outcome::ClaudeStillRunning(pending));
    settle(&mut harness);
    harness.get_by_label("Claude chưa thoát");
    assert!(harness.state().jobs.is_empty());

    harness.get_by_label("Buộc thoát và tiếp tục").click();
    settle(&mut harness);
    assert_eq!(
        harness.state().jobs,
        [Job::Switch {
            id: "home".into(),
            force_quit: true
        }]
    );
}

#[test]
fn saving_without_a_login_needs_consent() {
    let mut harness = open(two_accounts());
    let pending = Job::SaveCurrent {
        label: "X".into(),
        force_quit: false,
        allow_unverified: false,
    };
    harness.state_mut().view.job_started();
    harness
        .state_mut()
        .view
        .job_finished(Outcome::NotLoggedIn(pending));
    settle(&mut harness);
    harness.get_by_label("Chưa thấy phiên đăng nhập");
    harness.get_by_label("Vẫn lưu").click();
    settle(&mut harness);
    assert_eq!(
        harness.state().jobs,
        [Job::SaveCurrent {
            label: "X".into(),
            force_quit: false,
            allow_unverified: true
        }]
    );
}

#[test]
fn nothing_can_be_started_while_a_job_runs() {
    let mut harness = open(two_accounts());
    harness.state_mut().view.job_started();
    harness
        .state_mut()
        .view
        .set_progress("Đang thoát Claude…".into());
    settle(&mut harness);
    harness.get_by_label("Đang thoát Claude…");
    harness.get_by_label("Chuyển").click();
    harness.get_by_label("+  Thêm tài khoản").click();
    settle(&mut harness);
    assert!(harness.state().jobs.is_empty());
}

#[test]
fn results_and_errors_are_shown_and_can_be_dismissed() {
    let mut harness = open(two_accounts());
    harness.state_mut().view.job_started();
    harness
        .state_mut()
        .view
        .job_finished(Outcome::Done("Đã chuyển sang “Cá nhân”.".into()));
    settle(&mut harness);
    harness.get_by_label("Đã chuyển sang “Cá nhân”.");
    harness.get_by_label("Đóng").click();
    settle(&mut harness);
    assert!(
        harness
            .query_by_label("Đã chuyển sang “Cá nhân”.")
            .is_none()
    );

    harness
        .state_mut()
        .view
        .job_finished(Outcome::Failed("Không ghi được tệp.".into()));
    settle(&mut harness);
    harness.get_by_label("Không ghi được tệp.");
}

#[test]
fn a_logged_out_claude_can_be_opened_from_here() {
    let mut harness = open(overview(LiveSession::LoggedOut, false, Vec::new()));
    harness.get_by_label("Chưa có tài khoản nào được lưu.");
    harness.get_by_label("Mở Claude").click();
    settle(&mut harness);
    assert_eq!(harness.state().jobs, [Job::OpenClaude]);
}

#[test]
fn a_missing_claude_install_is_explained() {
    let mut missing = overview(LiveSession::NoDataDir, false, Vec::new());
    missing.data_dir = None;
    let mut harness = open(missing);
    harness.get_by_label("KHÔNG TÌM THẤY CLAUDE DESKTOP");
    // Adding an account makes no sense without Claude.
    harness.get_by_label("+  Thêm tài khoản").click();
    settle(&mut harness);
    assert!(harness.query_by_label("Thêm tài khoản khác").is_none());
}

#[test]
fn a_startup_failure_is_shown_instead_of_an_empty_window() {
    let mut view = View::default();
    view.set_overview(Err(
        "Ứng dụng này đang mở ở một cửa sổ khác. Hãy dùng cửa sổ đó.".into(),
    ));
    let mut harness = harness(
        Screen {
            view,
            jobs: Vec::new(),
        },
        Theme::Light,
    );
    settle(&mut harness);
    harness.get_by_label("Ứng dụng này đang mở ở một cửa sổ khác. Hãy dùng cửa sổ đó.");
    // Nothing else is offered while the state is unknown.
    assert!(harness.query_by_label("+  Thêm tài khoản").is_none());
}

/// What the user's own first test ran into: "dev" was logged out inside Claude, "cty" is in
/// use, and Claude is at its login screen after refusing the old session of "dev".
fn after_a_log_out_inside_claude() -> Overview {
    let mut dev = account("dev", "dev", false, logged_in(26));
    dev.account.signed_out = true;
    overview(
        LiveSession::LoggedOut,
        true,
        vec![dev, account("cty", "cty", false, logged_in(28))],
    )
}

#[test]
fn a_signed_out_account_offers_to_log_in_again() {
    let mut harness = open(after_a_log_out_inside_claude());
    harness.get_by_label("CLAUDE CHƯA ĐĂNG NHẬP");
    harness.get_by_label("Đã bị đăng xuất khỏi Claude");
    // The other account still switches as usual; exactly one of each button exists.
    harness.get_by_label("Chuyển");
    // Claude is already running, so there is nothing to open.
    assert!(harness.query_by_label("Mở Claude").is_none());

    harness.get_by_label("Đăng nhập lại").click();
    settle(&mut harness);
    assert_eq!(
        harness.state().jobs,
        [Job::Switch {
            id: "dev".into(),
            force_quit: false
        }]
    );
}

#[test]
fn says_so_the_moment_claude_signs_a_saved_account_out() {
    let mut harness = open(two_accounts());
    let mut signed_out = two_accounts();
    signed_out.live = LiveSession::LoggedOut;
    signed_out.accounts[0].is_live = false;
    signed_out.accounts[0].account.signed_out = true;

    harness
        .state_mut()
        .view
        .set_overview(Ok(signed_out.clone()));
    settle(&mut harness);
    let notice = "Claude đã đăng xuất “Công việc” nên phiên đã lưu của tài khoản này không còn \
                  dùng được. Hãy đăng nhập lại “Công việc” trong Claude.";
    harness.get_by_label(notice);

    // Said once: dismissing it is final while nothing new happens.
    harness.get_by_label("Đóng").click();
    harness.state_mut().view.set_overview(Ok(signed_out));
    settle(&mut harness);
    assert!(harness.query_by_label(notice).is_none());
}

#[test]
fn an_account_already_signed_out_at_startup_raises_no_alarm() {
    let harness = open(after_a_log_out_inside_claude());
    assert!(harness.query_by_label("Đóng").is_none());
}

/// Paint the main screens to PNG files for visual review.
#[test]
fn renders_previews() {
    let out_dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("ui-preview");
    std::fs::create_dir_all(&out_dir).unwrap();

    let many = || {
        overview(
            LiveSession::Saved { id: "work".into() },
            true,
            vec![
                account("work", "Công việc", true, logged_in(27)),
                account("home", "Cá nhân", false, logged_in(12)),
                account(
                    "team",
                    "Nhóm dự án Ánh Dương — tài khoản dùng chung của cả phòng",
                    false,
                    logged_in(2),
                ),
                account("old", "Thử nghiệm", false, logged_in(-3)),
            ],
        )
    };

    let mut screens: Vec<(&str, Theme, Screen)> = Vec::new();

    let mut done = screen(many());
    done.view
        .job_finished(Outcome::Done("Đã chuyển sang “Công việc”.".into()));
    screens.push(("main-light", Theme::Light, done));
    screens.push(("main-dark", Theme::Dark, screen(many())));

    let unsaved = LiveSession::Unsaved { account_uuid: None };
    let one = vec![account("work", "Công việc", false, logged_in(27))];
    screens.push((
        "unsaved-light",
        Theme::Light,
        screen(overview(unsaved.clone(), true, one.clone())),
    ));
    screens.push((
        "unsaved-dark",
        Theme::Dark,
        screen(overview(unsaved, true, one)),
    ));
    screens.push((
        "empty-light",
        Theme::Light,
        screen(overview(LiveSession::LoggedOut, false, Vec::new())),
    ));

    let mut busy = screen(many());
    busy.view.job_started();
    busy.view
        .set_progress("Đang nạp phiên của “Cá nhân”…".into());
    screens.push(("busy-dark", Theme::Dark, busy));

    let mut nobody_live = many().accounts;
    nobody_live[0].is_live = false;
    let mut failed = screen(overview(LiveSession::Interrupted, false, nobody_live));
    failed.view.job_finished(Outcome::Failed(
        "Claude đã được mở lại trong lúc đang đổi tài khoản. Hãy bấm Chuyển lần nữa.".into(),
    ));
    screens.push(("error-light", Theme::Light, failed));

    let mut lost = screen(two_accounts());
    let mut signed_out = after_a_log_out_inside_claude();
    signed_out.accounts[0].account.id = "work".into();
    signed_out.accounts[0].account.label = "Công việc".into();
    lost.view.set_overview(Ok(signed_out));
    screens.push(("signed-out-light", Theme::Light, lost));
    screens.push((
        "signed-out-dark",
        Theme::Dark,
        screen(after_a_log_out_inside_claude()),
    ));

    let mut stuck = screen(many());
    stuck
        .view
        .job_finished(Outcome::ClaudeStillRunning(Job::Switch {
            id: "home".into(),
            force_quit: false,
        }));
    screens.push(("dialog-force-quit-dark", Theme::Dark, stuck));

    let mut painted = 0;
    for (name, theme, screen) in screens {
        let mut harness = harness(screen, theme);
        settle(&mut harness);
        match harness.render() {
            Ok(image) => {
                image.save(out_dir.join(format!("{name}.png"))).unwrap();
                painted += 1;
            }
            // No GPU (some CI machines): the screens above were still laid out and checked.
            Err(error) => eprintln!("skipping the picture of {name}: {error}"),
        }
    }

    // The add dialog is reached by clicking, like a user would.
    let mut harness = open(two_accounts());
    harness.get_by_label("+  Thêm tài khoản").click();
    settle(&mut harness);
    if let Ok(image) = harness.render() {
        image.save(out_dir.join("dialog-add-light.png")).unwrap();
        painted += 1;
    }
    eprintln!("painted {painted} previews into {}", out_dir.display());
}
