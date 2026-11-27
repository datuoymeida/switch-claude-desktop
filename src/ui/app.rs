//! Glue between the window and the switching logic.
//!
//! Quitting Claude, copying a session and starting Claude again takes seconds, so every
//! job runs on one background thread and reports back through a channel. Running jobs one
//! at a time also guarantees that two operations never rewrite the session concurrently.

use eframe::egui;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::{Job, Outcome, View, style};
use crate::switcher::{OpError, Overview, Progress, SaveOptions, Switcher};

/// How often the state of Claude is re-read while the window is idle.
const REFRESH_EVERY: Duration = Duration::from_secs(2);

enum Event {
    Overview(Result<Overview, String>),
    Progress(String),
    Finished(Outcome),
}

pub struct App {
    view: View,
    /// `None` once the window is closing.
    jobs: Option<Sender<Job>>,
    events: Receiver<Event>,
    worker: Option<JoinHandle<()>>,
    last_refresh_request: Instant,
    refresh_pending: bool,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        style::apply(&cc.egui_ctx);

        let (jobs, job_queue) = channel();
        let (event_sink, events) = channel();
        let ctx = cc.egui_ctx.clone();
        let worker = std::thread::Builder::new()
            .name("switcher".to_owned())
            .spawn(move || worker(job_queue, event_sink, ctx))
            .expect("the worker thread can be started");

        let _ = jobs.send(Job::Refresh);
        Self {
            view: View::default(),
            jobs: Some(jobs),
            events,
            worker: Some(worker),
            last_refresh_request: Instant::now(),
            refresh_pending: true,
        }
    }

    fn send(&self, job: Job) {
        if let Some(jobs) = &self.jobs {
            let _ = jobs.send(job);
        }
    }

    fn drain_events(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                Event::Overview(overview) => {
                    self.refresh_pending = false;
                    self.view.set_overview(overview);
                }
                Event::Progress(text) => self.view.set_progress(text),
                Event::Finished(outcome) => self.view.job_finished(outcome),
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain_events();

        // Keep the picture current: Claude may be opened, closed or logged in at any time.
        let idle = !self.view.is_busy() && !self.refresh_pending;
        if idle && self.last_refresh_request.elapsed() >= REFRESH_EVERY {
            self.last_refresh_request = Instant::now();
            self.refresh_pending = true;
            self.send(Job::Refresh);
        }
        ui.ctx().request_repaint_after(REFRESH_EVERY);

        if let Some(job) = self.view.show(ui) {
            self.view.job_started();
            self.send(job);
        }
    }

    fn on_exit(&mut self) {
        // Closing the window in the middle of a switch must not abandon it half-way: stop
        // accepting jobs and let the one in progress finish before the process ends.
        self.jobs = None;
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn worker(jobs: Receiver<Job>, events: Sender<Event>, ctx: egui::Context) {
    let send = |event: Event| {
        let _ = events.send(event);
        ctx.request_repaint();
    };

    let switcher = match Switcher::for_this_machine() {
        Ok(switcher) => switcher,
        Err(error) => {
            // Nothing can work without the store; keep answering so the window says why.
            let text = format!("{error:#}");
            for job in jobs {
                send(Event::Overview(Err(text.clone())));
                if job != Job::Refresh {
                    send(Event::Finished(Outcome::Failed(text.clone())));
                }
            }
            return;
        }
    };
    let overview = || switcher.overview().map_err(|error| format!("{error:#}"));

    for job in jobs {
        if job == Job::Refresh {
            send(Event::Overview(overview()));
            continue;
        }
        let progress = |text: &str| send(Event::Progress(text.to_owned()));
        // A panic must not leave the window stuck on "working…".
        let outcome = catch_unwind(AssertUnwindSafe(|| run(&switcher, &job, &progress)))
            .unwrap_or_else(|_| {
                Outcome::Failed(String::from("Đã xảy ra lỗi không mong muốn. Hãy thử lại."))
            });
        // The new state first, so the list is already right when the message appears.
        send(Event::Overview(overview()));
        send(Event::Finished(outcome));
    }
}

fn run(switcher: &Switcher, job: &Job, progress: Progress<'_>) -> Outcome {
    let result = match job {
        Job::Refresh => return Outcome::Done(String::new()),
        Job::SaveCurrent {
            label,
            force_quit,
            allow_unverified,
        } => switcher.save_current(
            label,
            SaveOptions {
                force_quit: *force_quit,
                allow_unverified: *allow_unverified,
            },
            progress,
        ),
        Job::AddAccount { force_quit } => switcher.prepare_new_login(*force_quit, progress),
        Job::Switch { id, force_quit } => switcher.switch_to(id, *force_quit, progress),
        Job::Remove { id } => switcher.remove(id),
        Job::Rename { id, label } => switcher.rename(id, label),
        Job::OpenClaude => switcher.open_claude(),
    };
    match result {
        Ok(message) => Outcome::Done(message),
        Err(OpError::ClaudeStillRunning) => Outcome::ClaudeStillRunning(job.clone()),
        Err(OpError::NotLoggedIn) => Outcome::NotLoggedIn(job.clone()),
        Err(OpError::Failed(error)) => Outcome::Failed(format!("{error:#}")),
    }
}
