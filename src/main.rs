// RoboGUI v0.3.0 - folder backup tool with scheduled background runs.
//
//   robogui.exe              opens the window
//   robogui.exe --run <id>   runs one backup job without a window
//                            (this is what the scheduled tasks start)
#![cfg_attr(windows, windows_subsystem = "windows")]

mod engine;
mod image;
mod robo;
mod runner;
mod sched;
mod store;

use chrono::{Local, NaiveDateTime};
use eframe::egui::{self, Color32, RichText};
use engine::{Backup, Event};
use robo::{help, human_bytes, Purpose};
use runner::Control;
use sched::TaskInfo;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};
use store::{Config, Job, Method, Repeat, RunRecord, State, Weekday};

const VERSION: &str = env!("CARGO_PKG_VERSION");

// ---------------------------------------------------------------------------
// Time wording
// ---------------------------------------------------------------------------

fn now() -> NaiveDateTime {
    Local::now().naive_local()
}

/// "today at 04:00", "yesterday at 04:00", "on Mon 16 Nov at 04:00"
fn when_past(t: NaiveDateTime, now: NaiveDateTime) -> String {
    let hm = t.format("%H:%M");
    let days = (now.date() - t.date()).num_days();
    match days {
        0 => format!("today at {hm}"),
        1 => format!("yesterday at {hm}"),
        _ => format!("on {} at {hm}", t.format("%a %-d %b")),
    }
}

/// "today at 04:00", "tomorrow at 04:00", "on Sunday at 04:00"
fn when_future(t: NaiveDateTime, now: NaiveDateTime) -> String {
    let hm = t.format("%H:%M");
    match (t.date() - now.date()).num_days() {
        0 => format!("today at {hm}"),
        1 => format!("tomorrow at {hm}"),
        _ => format!("on {} at {hm}", t.format("%A")),
    }
}

/// List label: "Today, 04:00", "Yesterday, 04:00", "Mon 16 Nov, 04:00"
fn list_label(t: NaiveDateTime, now: NaiveDateTime) -> String {
    let hm = t.format("%H:%M");
    match (now.date() - t.date()).num_days() {
        0 => format!("Today, {hm}"),
        1 => format!("Yesterday, {hm}"),
        _ => format!("{}, {hm}", t.format("%a %-d %b")),
    }
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Tone {
    Ok,
    Warn,
    Error,
    Neutral,
}

fn tone_colors(t: Tone, dark: bool) -> (Color32, Color32) {
    let c = |r: u32| Color32::from_rgb((r >> 16) as u8, (r >> 8) as u8, r as u8);
    match (t, dark) {
        (Tone::Ok, true) => (c(0x1d3527), c(0x8fd6a5)),
        (Tone::Ok, false) => (c(0xe3f3e8), c(0x1e6b3a)),
        (Tone::Warn, true) => (c(0x3b3118), c(0xf0c46a)),
        (Tone::Warn, false) => (c(0xfbf0d9), c(0x8a5a00)),
        (Tone::Error, true) => (c(0x45201f), c(0xf19a96)),
        (Tone::Error, false) => (c(0xfbe6e5), c(0xa32d2d)),
        (Tone::Neutral, true) => (c(0x2b2b2b), c(0xd6d6d6)),
        (Tone::Neutral, false) => (c(0xefefef), c(0x3a3a3a)),
    }
}

struct Status {
    tone: Tone,
    title: String,
    detail: String,
    short: String, // sidebar
    /// Confirmation line about the last scheduled (background) run.
    scheduled: String,
}

fn job_status(job: &Job, state: &State, task: Option<&TaskInfo>, now: NaiveDateTime) -> Status {
    let scheduled = if job.schedule.enabled || state.last_scheduled().is_some() {
        match state.last_scheduled() {
            Some(r) => {
                let t = r.finished_time().map(|t| when_past(t, now)).unwrap_or_default();
                let res = if !r.ok { "failed" } else if r.warnings > 0 { "finished with problems" } else { "OK" };
                format!("Last scheduled run: {t}, {res}")
            }
            None => "No scheduled run yet".into(),
        }
    } else {
        String::new()
    };
    let mk = |tone, title: String, detail: String, short: &str| Status { tone, title, detail, short: short.to_string(), scheduled: scheduled.clone() };
    if let Some(p) = job.problem() {
        return mk(Tone::Neutral, "Not set up yet".into(), p, "Not set up");
    }
    let next = if job.schedule.enabled {
        sched::next_run(&job.schedule, now).map(|t| format!("Next backup {}", when_future(t, now))).unwrap_or_default()
    } else {
        "Automatic backup is off".into()
    };
    let last = state.last();
    let last_time = last.and_then(|r| r.finished_time());

    if !engine::dest_drive_available(&job.dest) {
        return mk(
            Tone::Warn,
            format!("Backup drive {} isn't connected", engine::drive_label(&job.dest)),
            "Backups can't run until it's back.".into(),
            "Drive missing",
        );
    }
    if job.schedule.enabled && task.is_some_and(|t| !t.exists) {
        return mk(
            Tone::Warn,
            "The scheduled task is missing in Windows".into(),
            "Turn automatic backup off and on again to recreate it.".into(),
            "Task missing",
        );
    }
    let Some(last) = last else {
        return mk(Tone::Neutral, "Not backed up yet".into(), format!("Press Back up now to make the first backup. {next}."), "Never run");
    };
    let when = last_time.map(|t| when_past(t, now)).unwrap_or_default();
    let short_when = last_time.map(|t| list_label(t, now)).unwrap_or_default();
    if !last.ok {
        return mk(Tone::Error, format!("Last backup failed {when}"), last.message.clone(), "Failed");
    }
    if job.schedule.enabled {
        let enabled_since = NaiveDateTime::parse_from_str(&job.schedule.enabled_at, "%Y-%m-%d %H:%M:%S").ok();
        if let Some(due) = sched::previous_due(&job.schedule, now) {
            if sched::missed(due, last_time, now, enabled_since) {
                return mk(
                    Tone::Warn,
                    format!("The backup due {} didn't run", when_past(due, now)),
                    format!("Last backup was {when}. Was the PC switched off? {next}."),
                    "Missed",
                );
            }
        }
    }
    let how = if last.scheduled { "by schedule" } else { "manually" };
    if last.warnings > 0 {
        return mk(Tone::Warn, format!("Backed up {when}, with problems"), last.message.clone(), &short_when);
    }
    mk(
        Tone::Ok,
        format!("Backed up {when}"),
        format!("{} Â· {how} Â· {next}", if last.size > 0 { human_bytes(last.size) } else { last.message.clone() }),
        &short_when,
    )
}

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Field {
    Name,
    Source,
    Dest,
    How,
    When,
    Keep,
    Exclude,
}

enum Msg {
    Ev(Event),
    BackupDone(Option<RunRecord>),
    RestoreDone(Result<PathBuf, String>),
}

#[derive(PartialEq)]
enum RunKind {
    Backup,
    Restore,
}

struct Running {
    job_id: String,
    kind: RunKind,
    rx: Receiver<Msg>,
    ctl: Control,
    phase: String,
    total: Option<(u64, u64)>,
    done: (u64, u64),
}

struct App {
    cfg: Config,
    states: HashMap<String, State>,
    tasks: HashMap<String, TaskInfo>,
    sel: Option<usize>,
    edit: Option<Field>,
    run: Option<Running>,
    live_log: VecDeque<String>,
    notice: Option<(Tone, String)>,
    restored: Option<PathBuf>,
    backups: Option<(String, Vec<Backup>, Instant)>,
    confirm_delete: bool,
    open_details: bool,
    last_poll: Instant,
    resolve_lib: Option<PathBuf>,
    /// This window runs with administrator rights.
    elevated: bool,
    /// Progress of runs going on in another process (scheduled task), by job id.
    bg: HashMap<String, String>,
}

impl App {
    fn new() -> Self {
        let cfg = store::load_config();
        let sel = cfg
            .selected
            .as_ref()
            .and_then(|id| cfg.jobs.iter().position(|j| &j.id == id))
            .or(if cfg.jobs.is_empty() { None } else { Some(0) });
        let mut app = App {
            cfg,
            states: HashMap::new(),
            tasks: HashMap::new(),
            sel,
            edit: None,
            run: None,
            live_log: VecDeque::new(),
            notice: None,
            restored: None,
            backups: None,
            confirm_delete: false,
            open_details: false,
            last_poll: Instant::now(),
            resolve_lib: store::find_resolve_library(),
            elevated: image::is_elevated(),
            bg: HashMap::new(),
        };
        app.reload_states();
        app.check_tasks();
        app
    }

    fn reload_states(&mut self) {
        self.bg.clear();
        let own = self.run.as_ref().map(|r| r.job_id.clone());
        for j in &self.cfg.jobs {
            self.states.insert(j.id.clone(), store::load_state(&j.id));
            if own.as_deref() != Some(j.id.as_str()) {
                if let Some(p) = store::background_progress(&j.id) {
                    self.bg.insert(j.id.clone(), p);
                }
            }
        }
    }

    /// Ask Task Scheduler about every scheduled job. If a task is missing
    /// or points at an old copy of this program (exe moved), re-register it.
    fn check_tasks(&mut self) {
        let exe = std::env::current_exe().map(|p| p.to_string_lossy().to_lowercase()).unwrap_or_default();
        let mut changed = false;
        for j in self.cfg.jobs.iter_mut() {
            if !(j.schedule.enabled && j.is_complete()) {
                continue;
            }
            let mut info = sched::query(&j.task_name);
            if !info.exists || !info.command.to_lowercase().contains(&exe) {
                if sched::sync(j).is_ok() {
                    changed = true;
                    info = sched::query(&j.task_name);
                }
            }
            self.tasks.insert(j.id.clone(), info);
        }
        if changed {
            self.save();
        }
    }

    fn save(&mut self) {
        self.cfg.selected = self.sel.and_then(|i| self.cfg.jobs.get(i)).map(|j| j.id.clone());
        if let Err(e) = store::save_config(&self.cfg) {
            self.notice = Some((Tone::Error, format!("Couldn't save settings: {e}")));
        }
    }

    /// Save, and bring Task Scheduler in line with job `i`.
    fn commit(&mut self, i: usize) {
        let j = &mut self.cfg.jobs[i];
        let res = sched::sync(j);
        let id = j.id.clone();
        let name = j.task_name.clone();
        match res {
            Ok(Some(msg)) => self.notice = Some((Tone::Warn, msg)),
            Ok(None) => {}
            Err(e) => self.notice = Some((Tone::Error, e)),
        }
        self.tasks.insert(id, sched::query(&name));
        self.backups = None;
        self.save();
    }

    fn busy(&self) -> bool {
        self.run.is_some()
    }

    fn start_backup(&mut self, ctx: &egui::Context, i: usize) {
        let job = self.cfg.jobs[i].clone();
        let (tx, rx) = channel();
        let ctl = Control::default();
        let c2 = ctl.clone();
        let ctx2 = ctx.clone();
        std::thread::spawn(move || {
            let txe = tx.clone();
            let r = engine::execute(&job, false, &c2, &mut |e| {
                let _ = txe.send(Msg::Ev(e));
            });
            let _ = tx.send(Msg::BackupDone(r));
            ctx2.request_repaint();
        });
        self.live_log.clear();
        self.restored = None;
        self.notice = None;
        self.run = Some(Running { job_id: self.cfg.jobs[i].id.clone(), kind: RunKind::Backup, rx, ctl, phase: "Startingâ€¦".into(), total: None, done: (0, 0) });
    }

    /// "Back up now". A drive image needs admin: without it, start the job's
    /// scheduled task instead (it has the rights) and follow its progress.
    fn back_up_now(&mut self, ctx: &egui::Context, i: usize) {
        let job = &self.cfg.jobs[i];
        if job.method != Method::Image || self.elevated {
            self.start_backup(ctx, i);
            return;
        }
        let id = job.id.clone();
        let has_task = self.tasks.get(&id).is_some_and(|t| t.exists);
        if !has_task {
            self.notice = Some((
                Tone::Warn,
                "A drive image needs administrator rights. Press \"Open as administrator\", or turn on automatic backup there once; after that, Back up now works from here too.".into(),
            ));
            return;
        }
        match sched::run_now(&job.task_name) {
            Ok(()) => {
                self.notice = Some((Tone::Ok, "Started in the background with administrator rights. Progress shows here; you can close RoboGUI.".into()));
                self.bg.insert(id, "Startingâ€¦".into());
                self.last_poll = Instant::now();
            }
            Err(e) => self.notice = Some((Tone::Error, format!("Couldn't start the scheduled task: {e}"))),
        }
    }

    fn start_restore(&mut self, ctx: &egui::Context, i: usize, zip: PathBuf, into: PathBuf) {
        let (tx, rx) = channel();
        let ctl = Control::default();
        let c2 = ctl.clone();
        let ctx2 = ctx.clone();
        std::thread::spawn(move || {
            let txe = tx.clone();
            let r = engine::restore(&zip, &into, &c2, &mut |e| {
                let _ = txe.send(Msg::Ev(e));
            });
            let _ = tx.send(Msg::RestoreDone(r));
            ctx2.request_repaint();
        });
        self.live_log.clear();
        self.restored = None;
        self.notice = None;
        self.run = Some(Running { job_id: self.cfg.jobs[i].id.clone(), kind: RunKind::Restore, rx, ctl, phase: "Opening backupâ€¦".into(), total: None, done: (0, 0) });
    }

    fn poll(&mut self) {
        // Pick up runs finished by the scheduler while the window is open.
        let every = if self.bg.is_empty() { 5 } else { 2 };
        if self.last_poll.elapsed() > Duration::from_secs(every) {
            self.last_poll = Instant::now();
            if !self.busy() {
                self.reload_states();
                if let Some((_, _, t)) = &self.backups {
                    if t.elapsed() > Duration::from_secs(30) {
                        self.backups = None;
                    }
                }
            }
        }
        let Some(r) = &mut self.run else { return };
        let mut finished: Option<Msg> = None;
        let mut lines = Vec::new();
        for m in r.rx.try_iter() {
            match m {
                Msg::Ev(Event::Phase(p)) => r.phase = p,
                Msg::Ev(Event::Line(l)) => lines.push(l),
                Msg::Ev(Event::Total { files, bytes }) => r.total = Some((files, bytes)),
                Msg::Ev(Event::Progress { files, bytes }) => r.done = (files, bytes),
                done => finished = Some(done),
            }
        }
        for l in lines {
            if self.live_log.len() > 3000 {
                self.live_log.pop_front();
            }
            self.live_log.push_back(l);
        }
        match finished {
            Some(Msg::BackupDone(rec)) => {
                let id = self.run.take().unwrap().job_id;
                match rec {
                    None => self.notice = Some((Tone::Warn, "This job is already running (probably the scheduled backup). Try again when it's done.".into())),
                    Some(r) if !r.ok => self.open_details = true,
                    Some(_) => {}
                }
                self.states.insert(id.clone(), store::load_state(&id));
                self.backups = None;
            }
            Some(Msg::RestoreDone(res)) => {
                self.run = None;
                match res {
                    Ok(p) => self.restored = Some(p),
                    Err(e) => self.notice = Some((Tone::Error, format!("Restore failed: {e}"))),
                }
            }
            _ => {}
        }
    }

    fn new_job(&mut self, preset_resolve: bool) {
        let mut j = Job::default();
        if preset_resolve {
            if let Some(p) = &self.resolve_lib {
                j.name = "Resolve project library".into();
                j.prefix = "ResolveDB".into();
                j.source = p.display().to_string();
                j.schedule.time = "04:00".into();
            }
        }
        self.cfg.jobs.push(j);
        self.sel = Some(self.cfg.jobs.len() - 1);
        self.edit = Some(if preset_resolve { Field::Dest } else { Field::Source });
        self.backups = None;
        self.save();
    }
}

// ---------------------------------------------------------------------------
// Widgets
// ---------------------------------------------------------------------------

fn pick_folder(current: &str, title: &str) -> Option<String> {
    let mut d = rfd::FileDialog::new().set_title(title);
    let c = current.trim();
    if !c.is_empty() && std::path::Path::new(c).is_dir() {
        d = d.set_directory(c);
    }
    d.pick_folder().map(|p| p.display().to_string())
}

fn open_in_explorer(path: &str) {
    let _ = std::process::Command::new("explorer.exe").arg(path).spawn();
}

/// "Label    value                                Change"
fn row(ui: &mut egui::Ui, label: &str, value: impl Into<RichText>, hover: Option<&str>, editing: bool) -> bool {
    let mut clicked = false;
    ui.horizontal(|ui| {
        ui.allocate_ui_with_layout(egui::vec2(90.0, 20.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
            ui.set_min_width(90.0);
            ui.label(RichText::new(label).color(ui.visuals().weak_text_color()));
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.link(if editing { "Done" } else { "Change" }).clicked() {
                clicked = true;
            }
            ui.add_space(12.0);
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                let r = ui.add(egui::Label::new(value.into()).truncate());
                if let Some(h) = hover {
                    r.on_hover_text(h);
                }
            });
        });
    });
    clicked
}

fn editor_frame(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::group(ui.style()).inner_margin(egui::Margin::same(10)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        add(ui)
    });
    ui.add_space(4.0);
}

fn checkbox_help(ui: &mut egui::Ui, v: &mut bool, label: &str, help: &str) {
    ui.checkbox(v, label).on_hover_text(help);
}

// ---------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------

impl App {
    fn sidebar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.label(RichText::new("Backup jobs").color(ui.visuals().weak_text_color()));
        ui.add_space(4.0);
        let now = now();
        let dark = ui.visuals().dark_mode;
        let mut clicked = None;
        for (i, j) in self.cfg.jobs.iter().enumerate() {
            let st = job_status(j, self.states.get(&j.id).unwrap_or(&State::default()), self.tasks.get(&j.id), now);
            let selected = self.sel == Some(i);
            let fill = if selected { ui.visuals().selection.bg_fill.gamma_multiply(0.35) } else { Color32::TRANSPARENT };
            let resp = egui::Frame::new().fill(fill).corner_radius(6.0).inner_margin(egui::Margin::symmetric(8, 6)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new(&j.name).strong());
                let (_, fg) = tone_colors(st.tone, dark);
                let busy_text = self
                    .run
                    .as_ref()
                    .filter(|r| r.job_id == j.id)
                    .map(|r| if r.kind == RunKind::Restore { "Restoringâ€¦" } else { "Backing upâ€¦" })
                    .or_else(|| self.bg.contains_key(&j.id).then_some("Backing upâ€¦"));
                ui.horizontal(|ui| {
                    if let Some(t) = busy_text {
                        ui.add(egui::Spinner::new().size(10.0));
                        ui.label(RichText::new(t).size(12.5));
                    } else {
                        let (rect, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
                        ui.painter().circle_filled(rect.center(), 4.0, fg);
                        ui.label(RichText::new(&st.short).color(fg).size(12.5));
                    }
                });
            });
            if resp.response.interact(egui::Sense::click()).clicked() {
                clicked = Some(i);
            }
        }
        if let Some(i) = clicked {
            if self.sel != Some(i) {
                self.sel = Some(i);
                self.edit = None;
                self.backups = None;
                self.restored = None;
                self.notice = None;
                self.confirm_delete = false;
                self.save();
            }
        }
        ui.add_space(8.0);
        if ui.button("+  New backup job").clicked() {
            self.new_job(false);
        }
    }

    fn welcome(&mut self, ui: &mut egui::Ui) {
        ui.add_space(40.0);
        ui.vertical_centered(|ui| {
            ui.heading("Back up your first folder");
            ui.add_space(6.0);
            ui.label("Pick a folder, where to save it and how often. Scheduled backups run in the background, even when RoboGUI is closed.");
            ui.add_space(16.0);
            if self.resolve_lib.is_some() && ui.button("Back up the DaVinci Resolve project library").clicked() {
                self.new_job(true);
            }
            ui.add_space(6.0);
            if ui.button("Choose another folderâ€¦").clicked() {
                self.new_job(false);
            }
        });
    }

    fn status_card(&mut self, ui: &mut egui::Ui, i: usize) {
        let now = now();
        let job = &self.cfg.jobs[i];
        let dark = ui.visuals().dark_mode;
        let running = self.run.as_ref().filter(|r| r.job_id == job.id);
        let background = self.bg.get(&job.id).cloned();
        let st = job_status(job, self.states.get(&job.id).unwrap_or(&State::default()), self.tasks.get(&job.id), now);
        let tone = if running.is_some() || background.is_some() { Tone::Neutral } else { st.tone };
        let (bg, fg) = tone_colors(tone, dark);
        let can_run = job.problem().is_none() && !self.busy() && background.is_none();
        let mut start = false;
        let mut stop = false;
        egui::Frame::new().fill(bg).corner_radius(8.0).inner_margin(egui::Margin::same(14)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            if let Some(r) = running {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new(&r.phase).color(fg).size(16.0));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Stop").clicked() {
                            stop = true;
                        }
                    });
                });
                if let Some((files, bytes)) = r.total {
                    let frac = if bytes > 0 { r.done.1 as f32 / bytes as f32 } else if files > 0 { r.done.0 as f32 / files as f32 } else { 1.0 };
                    // Drive images report a percentage only (total 100, no files).
                    let text = if files == 0 && bytes == 100 {
                        format!("{}%", r.done.1)
                    } else {
                        format!("{} of {} files Â· {} of {}", r.done.0, files, human_bytes(r.done.1), human_bytes(bytes))
                    };
                    ui.add(egui::ProgressBar::new(frac.clamp(0.0, 1.0)).text(text));
                }
            } else if let Some(p) = &background {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.vertical(|ui| {
                        ui.label(RichText::new("Backing up in the background").color(fg).size(16.0).strong());
                        ui.label(RichText::new(p).color(fg));
                    });
                });
                let pct = p.rsplit(' ').next().and_then(|w| w.strip_suffix('%')).and_then(|n| n.parse::<f32>().ok());
                if let Some(pct) = pct {
                    ui.add(egui::ProgressBar::new((pct / 100.0).clamp(0.0, 1.0)).text(format!("{pct}%")));
                }
                ui.label(RichText::new("Started by Windows Task Scheduler. You can close RoboGUI; the result shows up here when it's done.").color(fg).size(12.5));
            } else {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(RichText::new(&st.title).color(fg).size(16.0).strong());
                        ui.label(RichText::new(&st.detail).color(fg));
                        if !st.scheduled.is_empty() {
                            ui.label(RichText::new(&st.scheduled).color(fg).size(13.0))
                                .on_hover_text("Runs started by Windows Task Scheduler in the background, read from the job's run history.");
                        }
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let b = ui.add_enabled(can_run, egui::Button::new("Back up now").min_size(egui::vec2(110.0, 30.0)));
                        if b.clicked() {
                            start = true;
                        }
                    });
                });
            }
        });
        if start {
            self.back_up_now(ui.ctx(), i);
        }
        if stop {
            if let Some(r) = &self.run {
                r.ctl.stop();
            }
        }
    }

    fn job_view(&mut self, ui: &mut egui::Ui, i: usize) {
        // Header
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.heading(&self.cfg.jobs[i].name);
        });
        ui.add_space(6.0);
        self.status_card(ui, i);
        if self.cfg.jobs[i].method == Method::Image && !self.elevated {
            let has_task = self.tasks.get(&self.cfg.jobs[i].id).is_some_and(|t| t.exists);
            ui.horizontal_wrapped(|ui| {
                let text = if has_task {
                    "Drive images run with administrator rights through the schedule. To change this job's settings, open RoboGUI as administrator."
                } else {
                    "A drive image needs administrator rights to set up. Open RoboGUI as administrator once and turn on automatic backup; after that it runs on its own."
                };
                ui.label(RichText::new(text).weak());
                if ui.button("Open as administrator").clicked() {
                    self.save();
                    if image::relaunch_as_admin() {
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    } else {
                        self.notice = Some((Tone::Warn, "Windows didn't start RoboGUI as administrator.".into()));
                    }
                }
            });
        }
        if let Some((tone, text)) = self.notice.clone() {
            let (_, fg) = tone_colors(tone, ui.visuals().dark_mode);
            ui.horizontal_wrapped(|ui| {
                ui.colored_label(fg, text);
                if ui.small_button("Dismiss").clicked() {
                    self.notice = None;
                }
            });
        }
        if let Some(p) = self.restored.clone() {
            let is_resolve = self.cfg.jobs[i].source.contains("Resolve");
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(format!("Restored to {}", p.display()));
                if is_resolve {
                    ui.label(RichText::new("In Resolve: Project Manager > Project Libraries > Add, and choose the folder inside this one.").small());
                }
                ui.horizontal(|ui| {
                    if ui.button("Open folder").clicked() {
                        open_in_explorer(&p.display().to_string());
                    }
                    if ui.button("Close").clicked() {
                        self.restored = None;
                    }
                });
            });
        }
        ui.add_space(10.0);

        self.settings_rows(ui, i);

        ui.add_space(12.0);
        self.backups_section(ui, i);

        ui.add_space(12.0);
        self.history_section(ui, i);

        ui.add_space(6.0);
        self.details_section(ui, i);

        ui.add_space(16.0);
        ui.separator();
        if !self.confirm_delete {
            if ui.add_enabled(!self.busy(), egui::Button::new("Delete this job")).clicked() {
                self.confirm_delete = true;
            }
        } else {
            ui.label(format!(
                "Delete the job \"{}\"? Its schedule is removed from Windows. Backups already made stay where they are.",
                self.cfg.jobs[i].name
            ));
            ui.horizontal(|ui| {
                if ui.button("Delete job").clicked() {
                    let j = self.cfg.jobs.remove(i);
                    if !j.task_name.is_empty() {
                        let _ = sched::delete(&j.task_name);
                    }
                    self.sel = if self.cfg.jobs.is_empty() { None } else { Some(i.saturating_sub(1)) };
                    self.confirm_delete = false;
                    self.edit = None;
                    self.backups = None;
                    self.save();
                }
                if ui.button("Cancel").clicked() {
                    self.confirm_delete = false;
                }
            });
        }
    }

    fn settings_rows(&mut self, ui: &mut egui::Ui, i: usize) {
        let editing = self.edit;
        let toggle = |edit: &mut Option<Field>, f: Field| -> bool {
            // returns true when an edit was just closed (commit)
            if *edit == Some(f) {
                *edit = None;
                true
            } else {
                *edit = Some(f);
                false
            }
        };
        let mut commit = false;
        let resolve_lib = self.resolve_lib.clone();
        let job = &mut self.cfg.jobs[i];

        // Name
        if row(ui, "Name", job.name.as_str(), None, editing == Some(Field::Name)) {
            commit |= toggle(&mut self.edit, Field::Name);
        }
        if self.edit == Some(Field::Name) {
            editor_frame(ui, |ui| {
                let r = ui.add(egui::TextEdit::singleline(&mut job.name).desired_width(300.0));
                if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    self.edit = None;
                    commit = true;
                }
            });
        }

        // Source
        let src_text = if job.source.trim().is_empty() { RichText::new("Not chosen").italics() } else { RichText::new(job.source.as_str()) };
        if row(ui, "Back up", src_text, Some(job.source.as_str()), editing == Some(Field::Source)) {
            commit |= toggle(&mut self.edit, Field::Source);
        }
        if self.edit == Some(Field::Source) {
            editor_frame(ui, |ui| {
                ui.horizontal(|ui| {
                    let hint = if job.method == Method::Image { "Drive to back up, like C:" } else { "Folder to back up" };
                    ui.add(egui::TextEdit::singleline(&mut job.source).desired_width(ui.available_width() - 90.0).hint_text(hint));
                    if ui.button("Browseâ€¦").clicked() {
                        if let Some(p) = pick_folder(&job.source, hint) {
                            job.source = p;
                        }
                    }
                });
                if job.method == Method::Image {
                    if let Some(d) = store::drive_of(&job.source) {
                        job.source = d;
                    }
                    ui.label(RichText::new("The whole drive is imaged, plus whatever Windows needs to start (boot and recovery partitions).").small().weak());
                }
                if let Some(lib) = &resolve_lib {
                    let lib_s = lib.display().to_string();
                    if job.source != lib_s && ui.link("Use the DaVinci Resolve project library").on_hover_text(&lib_s).clicked() {
                        job.source = lib_s;
                    }
                }
                // Name new jobs after the folder
                if job.name == "New backup" {
                    if let Some(n) = std::path::Path::new(job.source.trim()).file_name() {
                        job.name = n.to_string_lossy().to_string();
                    }
                }
                if job.prefix == "Backup" && !job.source.trim().is_empty() {
                    if let Some(n) = std::path::Path::new(job.source.trim()).file_name() {
                        job.prefix = store::sanitize(&n.to_string_lossy());
                    }
                }
            });
        }

        // Destination
        let dst_text = if job.dest.trim().is_empty() { RichText::new("Not chosen").italics() } else { RichText::new(job.dest.as_str()) };
        if row(ui, "To", dst_text, Some(job.dest.as_str()), editing == Some(Field::Dest)) {
            commit |= toggle(&mut self.edit, Field::Dest);
        }
        if self.edit == Some(Field::Dest) {
            editor_frame(ui, |ui| {
                ui.horizontal(|ui| {
                    let hint = if job.method == Method::Image { "Backup drive, like H:" } else { "Where to save the backup" };
                    ui.add(egui::TextEdit::singleline(&mut job.dest).desired_width(ui.available_width() - 90.0).hint_text(hint));
                    if ui.button("Browseâ€¦").clicked() {
                        if let Some(p) = pick_folder(&job.dest, hint) {
                            job.dest = p;
                        }
                    }
                });
                if job.method == Method::Image {
                    if let Some(d) = store::drive_of(&job.dest) {
                        job.dest = d;
                    }
                    ui.label(RichText::new("Windows saves the image in a WindowsImageBackup folder on this drive. Best on a drive used only for backups.").small().weak());
                }
                if let Some(p) = job.problem() {
                    ui.colored_label(ui.visuals().warn_fg_color, p);
                }
            });
        }

        // Method
        let method_help = match job.method {
            Method::Zip => help::ZIP,
            Method::Mirror => help::MIRROR,
            Method::Copy => help::COPY,
            Method::Image => help::IMAGE,
        };
        if row(ui, "How", job.method.label(), Some(method_help), editing == Some(Field::How)) {
            commit |= toggle(&mut self.edit, Field::How);
        }
        if self.edit == Some(Field::How) {
            editor_frame(ui, |ui| {
                ui.radio_value(&mut job.method, Method::Zip, Method::Zip.label()).on_hover_text(help::ZIP);
                ui.radio_value(&mut job.method, Method::Mirror, Method::Mirror.label()).on_hover_text(help::MIRROR);
                ui.radio_value(&mut job.method, Method::Copy, Method::Copy.label()).on_hover_text(help::COPY);
                let was_image = job.method == Method::Image;
                ui.radio_value(&mut job.method, Method::Image, Method::Image.label()).on_hover_text(help::IMAGE);
                if job.method == Method::Image && !was_image {
                    // Start from sensible drives: the folder's drive (or C:) and the backup's drive.
                    job.source = store::drive_of(&job.source).unwrap_or_else(|| "C:".into());
                    job.dest = store::drive_of(&job.dest).unwrap_or_default();
                    if job.name == "New backup" || job.name.trim().is_empty() {
                        job.name = format!("{} drive image", job.source);
                    }
                }
                ui.add_space(4.0);
                match job.method {
                    Method::Zip => {
                        ui.label(RichText::new(format!("Files are named like {}_2026-11-18_0400.zip", job.prefix)).small().weak());
                    }
                    Method::Image => {
                        if let (Some(s), Some(d)) = (store::drive_of(&job.source), store::drive_of(&job.dest)) {
                            let cmd = format!("wbadmin {}", image::wbadmin_args(&s, &d).join(" "));
                            ui.label(RichText::new(cmd).monospace().small());
                        }
                        if !self.elevated {
                            ui.colored_label(ui.visuals().warn_fg_color, "Needs administrator rights to set up: use \"Open as administrator\" above.");
                        }
                    }
                    Method::Mirror | Method::Copy => {
                        if job.method == Method::Mirror {
                            ui.colored_label(ui.visuals().warn_fg_color, "Mirror deletes files in the backup that no longer exist in the source.");
                        }
                        ui.add_space(4.0);
                        ui.label(RichText::new("Robocopy options (hover for help)").strong());
                        let o = &mut job.robo;
                        ui.columns(2, |c| {
                            checkbox_help(&mut c[0], &mut o.skip_older, "Don't overwrite newer files", help::SKIP_OLDER);
                            checkbox_help(&mut c[0], &mut o.copy_dir_times, "Keep folder dates", help::DIR_TIMES);
                            checkbox_help(&mut c[0], &mut o.exclude_junctions, "Skip links and junctions", help::JUNCTIONS);
                            checkbox_help(&mut c[0], &mut o.restartable, "Resume interrupted files", help::RESTARTABLE);
                            checkbox_help(&mut c[0], &mut o.fat_times, "NAS / exFAT time tolerance", help::FAT_TIMES);
                            c[1].horizontal(|ui| {
                                checkbox_help(ui, &mut o.multithread, "Parallel copies", help::THREADS);
                                ui.add_enabled(o.multithread, egui::DragValue::new(&mut o.threads).range(1..=128)).on_hover_text(help::THREADS);
                            });
                            c[1].horizontal(|ui| {
                                ui.label("Retries").on_hover_text(help::RETRIES);
                                ui.add(egui::DragValue::new(&mut o.retries).range(0..=100)).on_hover_text(help::RETRIES);
                            });
                            c[1].horizontal(|ui| {
                                ui.label("Seconds between retries").on_hover_text(help::WAIT);
                                ui.add(egui::DragValue::new(&mut o.wait).range(0..=600)).on_hover_text(help::WAIT);
                            });
                        });
                        ui.horizontal(|ui| {
                            ui.label("Extra robocopy options").on_hover_text(help::EXTRA);
                            ui.add(egui::TextEdit::singleline(&mut o.extra).desired_width(240.0).hint_text("/COPY:DAT")).on_hover_text(help::EXTRA);
                        });
                        egui::CollapsingHeader::new("Show command").id_salt("cmd").show(ui, |ui| {
                            let cmd = format!("robocopy {}", engine::robo_args(job, Purpose::Display).join(" "));
                            ui.add(egui::Label::new(RichText::new(&cmd).monospace()).wrap());
                            if ui.small_button("Copy").clicked() {
                                ui.ctx().copy_text(cmd);
                            }
                        });
                    }
                }
            });
        }

        // Schedule
        let when_text = if job.schedule.enabled {
            let t = sched::parse_time(&job.schedule.time).unwrap_or_else(|| job.schedule.time.clone());
            match job.schedule.repeat {
                Repeat::Daily => format!("Every day at {t}"),
                Repeat::Weekly(d) => format!("Every {} at {t}", d.name()),
            }
        } else {
            "Only when I press Back up now".into()
        };
        let task_hint = "Runs as a Windows scheduled task, in the background, even when RoboGUI is closed. If the PC is off or asleep at that time, the backup runs as soon as it's back on.";
        if row(ui, "When", when_text, Some(task_hint), editing == Some(Field::When)) {
            commit |= toggle(&mut self.edit, Field::When);
        }
        if self.edit == Some(Field::When) {
            editor_frame(ui, |ui| {
                let was = job.schedule.enabled;
                ui.checkbox(&mut job.schedule.enabled, "Back up automatically").on_hover_text(task_hint);
                if job.schedule.enabled && !was {
                    job.schedule.enabled_at = now().format("%Y-%m-%d %H:%M:%S").to_string();
                }
                ui.add_enabled_ui(job.schedule.enabled, |ui| {
                    ui.horizontal(|ui| {
                        let mut weekly = matches!(job.schedule.repeat, Repeat::Weekly(_));
                        let mut day = match job.schedule.repeat {
                            Repeat::Weekly(d) => d,
                            Repeat::Daily => Weekday::Sunday,
                        };
                        ui.radio_value(&mut weekly, false, "Every day");
                        ui.radio_value(&mut weekly, true, "Every week on");
                        ui.add_enabled_ui(weekly, |ui| {
                            egui::ComboBox::from_id_salt("day").selected_text(day.name()).show_ui(ui, |ui| {
                                for d in Weekday::ALL {
                                    ui.selectable_value(&mut day, d, d.name());
                                }
                            });
                        });
                        job.schedule.repeat = if weekly { Repeat::Weekly(day) } else { Repeat::Daily };
                    });
                    ui.horizontal(|ui| {
                        ui.label("at");
                        ui.add(egui::TextEdit::singleline(&mut job.schedule.time).desired_width(50.0).hint_text("04:00"));
                        if sched::parse_time(&job.schedule.time).is_none() {
                            ui.colored_label(ui.visuals().warn_fg_color, "Use 24-hour time, like 04:00");
                        }
                    });
                });
                ui.label(RichText::new("Saved to Windows Task Scheduler when you press Done.").small().weak());
            });
        }

        // Retention (zip only)
        if job.method == Method::Zip {
            let keep_text = format!("Last {} backups and {} weekly copies", job.keep_daily.max(1), job.keep_weekly);
            let keep_help = "After each backup, older zips are deleted except:\nâ€¢ the newest N backups\nâ€¢ the newest backup from each of the last N Sundays\nSo with 7 and 4 you can go back a week day by day, and a month week by week.";
            if row(ui, "Keep", keep_text, Some(keep_help), editing == Some(Field::Keep)) {
                commit |= toggle(&mut self.edit, Field::Keep);
            }
            if self.edit == Some(Field::Keep) {
                editor_frame(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Newest");
                        ui.add(egui::DragValue::new(&mut job.keep_daily).range(1..=365));
                        ui.label("backups, plus");
                        ui.add(egui::DragValue::new(&mut job.keep_weekly).range(0..=104));
                        ui.label("weekly (Sunday) copies");
                    })
                    .response
                    .on_hover_text(keep_help);
                });
            }
        }
        if job.method == Method::Image {
            let d = store::drive_of(&job.dest).unwrap_or_else(|| "the backup drive".into());
            let keep_text = format!("As many versions as fit in {}% of {d}", job.image_space_pct);
            let keep_help = "Every image after the first only stores what changed. Older versions are kept until they fill this share of the backup drive; then Windows deletes the oldest first, so you always have the most recent ones.\nThe newest version is always a complete image.";
            if row(ui, "Keep", keep_text, Some(keep_help), editing == Some(Field::Keep)) {
                commit |= toggle(&mut self.edit, Field::Keep);
            }
            if self.edit == Some(Field::Keep) {
                editor_frame(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Older versions may use up to");
                        ui.add(egui::DragValue::new(&mut job.image_space_pct).range(10..=90).suffix(" %"));
                        ui.label(format!("of {d}"));
                    })
                    .response
                    .on_hover_text(keep_help);
                    ui.label(RichText::new("Applied at the start of each backup.").small().weak());
                });
            }
        }

        // Exclusions (a drive image always takes the whole drive)
        if job.method == Method::Image {
            if commit {
                self.commit(i);
            }
            return;
        }
        let ex_text = {
            let n = robo::split_list(&job.exclude_files).len() + robo::split_list(&job.exclude_dirs).len();
            if n == 0 { "Nothing".to_string() } else { format!("{n} rule(s)") }
        };
        if row(ui, "Leave out", ex_text, Some("Files and folders that are skipped"), editing == Some(Field::Exclude)) {
            commit |= toggle(&mut self.edit, Field::Exclude);
        }
        if self.edit == Some(Field::Exclude) {
            editor_frame(ui, |ui| {
                egui::Grid::new("ex").num_columns(2).show(ui, |ui| {
                    ui.label("Files").on_hover_text(help::EXCLUDE_FILES);
                    ui.add(egui::TextEdit::singleline(&mut job.exclude_files).desired_width(320.0).hint_text("*.tmp; Thumbs.db")).on_hover_text(help::EXCLUDE_FILES);
                    ui.end_row();
                    ui.label("Folders").on_hover_text(help::EXCLUDE_DIRS);
                    ui.add(egui::TextEdit::singleline(&mut job.exclude_dirs).desired_width(320.0).hint_text("CacheClip; .gallery")).on_hover_text(help::EXCLUDE_DIRS);
                    ui.end_row();
                });
            });
        }

        if commit {
            self.commit(i);
        }
    }

    fn backups_section(&mut self, ui: &mut egui::Ui, i: usize) {
        let job = self.cfg.jobs[i].clone();
        if !job.is_complete() {
            return;
        }
        if job.method == Method::Image {
            self.image_versions_section(ui, &job);
            return;
        }
        if job.method != Method::Zip {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Backup folder").strong());
                if ui.link("Open").clicked() {
                    open_in_explorer(job.dest.trim());
                }
            });
            return;
        }
        if self.backups.as_ref().is_none_or(|(id, _, _)| id != &job.id) {
            self.backups = Some((job.id.clone(), engine::list_backups(&job), Instant::now()));
        }
        let list = self.backups.as_ref().unwrap().1.clone();
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("Backups ({})", list.len())).strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.link("Show in folder").clicked() {
                    open_in_explorer(job.dest.trim());
                }
            });
        });
        if list.is_empty() {
            ui.label(RichText::new("No backups in this folder yet.").weak());
            return;
        }
        let now = now();
        let mut restore: Option<PathBuf> = None;
        egui::Frame::group(ui.style()).inner_margin(egui::Margin::symmetric(8, 4)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::Grid::new("bk").num_columns(4).striped(true).spacing([16.0, 6.0]).show(ui, |ui| {
                for b in list.iter().take(30) {
                    ui.label(list_label(b.time, now)).on_hover_text(b.path.display().to_string());
                    ui.label(RichText::new(human_bytes(b.size)).weak());
                    if b.weekly {
                        ui.label(RichText::new("weekly").small().weak()).on_hover_text("Kept as one of the weekly copies");
                    } else {
                        ui.label("");
                    }
                    if ui.add_enabled(!self.busy(), egui::Button::new("Restoreâ€¦").small())
                        .on_hover_text("Unzips this backup into a folder you choose. Nothing existing is overwritten.")
                        .clicked()
                    {
                        restore = Some(b.path.clone());
                    }
                    ui.end_row();
                }
            });
        });
        if let Some(zip) = restore {
            if let Some(into) = pick_folder("", "Restore into which folder? (a new subfolder is created)") {
                self.start_restore(ui.ctx(), i, zip, PathBuf::from(into));
            }
        }
    }

    /// Drive image versions on the backup drive, as saved by the last run,
    /// plus how full the drive is.
    fn image_versions_section(&mut self, ui: &mut egui::Ui, job: &Job) {
        let v = store::load_image_versions(&job.id);
        let d = store::drive_of(&job.dest).unwrap_or_default();
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("Image versions on {d} ({})", v.versions.len())).strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.link("Show drive").clicked() {
                    open_in_explorer(&format!("{d}\\"));
                }
            });
        });
        if let Some((free, total)) = image::disk_space(&d) {
            let used = total.saturating_sub(free);
            let frac = if total > 0 { used as f32 / total as f32 } else { 0.0 };
            ui.add(egui::ProgressBar::new(frac).text(format!("{d} {} used Â· {} free of {}", human_bytes(used), human_bytes(free), human_bytes(total))))
                .on_hover_text(format!("Older versions may use up to {}% of the drive. When that's full, the oldest are deleted first.", job.image_space_pct));
        }
        if v.versions.is_empty() {
            ui.label(RichText::new("No image versions recorded yet. The list is read after each backup.").weak());
            return;
        }
        let now = now();
        egui::Frame::group(ui.style()).inner_margin(egui::Margin::symmetric(8, 4)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::Grid::new(("iv", &job.id)).num_columns(2).striped(true).spacing([16.0, 6.0]).show(ui, |ui| {
                for (n, s) in v.versions.iter().take(30).enumerate() {
                    let t = NaiveDateTime::parse_from_str(&format!("{s}:00"), "%Y-%m-%d %H:%M:%S").ok();
                    ui.label(t.map(|t| list_label(t, now)).unwrap_or_else(|| s.clone()));
                    ui.label(RichText::new(if n == 0 { "newest" } else if n + 1 == v.versions.len() { "oldest, deleted first" } else { "" }).small().weak());
                    ui.end_row();
                }
            });
        });
        let checked = NaiveDateTime::parse_from_str(&v.checked, "%Y-%m-%d %H:%M:%S").ok().map(|t| when_past(t, now)).unwrap_or_default();
        ui.label(RichText::new(format!("Checked {checked}. To restore: Windows Recovery > Troubleshoot > System Image Recovery, or Disk Management > Attach VHD to get single files out.")).small().weak());
    }

    fn history_section(&mut self, ui: &mut egui::Ui, i: usize) {
        let id = self.cfg.jobs[i].id.clone();
        let hist = self.states.get(&id).map(|s| s.history.clone()).unwrap_or_default();
        let now = now();
        egui::CollapsingHeader::new(RichText::new("Run history").strong()).id_salt(("hist", &id)).default_open(true).show(ui, |ui| {
            if hist.is_empty() {
                ui.label(RichText::new("No runs yet. Scheduled runs show up here, even ones that ran while RoboGUI was closed.").weak());
                return;
            }
            let dark = ui.visuals().dark_mode;
            egui::Grid::new(("hg", &id)).num_columns(4).spacing([14.0, 4.0]).show(ui, |ui| {
                for r in hist.iter().take(10) {
                    let t = r.finished_time().map(|t| list_label(t, now)).unwrap_or_else(|| r.finished.clone());
                    ui.label(t);
                    ui.label(RichText::new(if r.scheduled { "Scheduled" } else { "Manual" }).weak());
                    let tone = if !r.ok { Tone::Error } else if r.warnings > 0 { Tone::Warn } else { Tone::Ok };
                    let (_, fg) = tone_colors(tone, dark);
                    ui.colored_label(fg, if !r.ok { "Failed" } else if r.warnings > 0 { "Problems" } else { "OK" });
                    ui.add(egui::Label::new(RichText::new(&r.message).weak()).truncate());
                    ui.end_row();
                }
            });
        });
    }

    fn details_section(&mut self, ui: &mut egui::Ui, i: usize) {
        let id = self.cfg.jobs[i].id.clone();
        let running_here = self.run.as_ref().is_some_and(|r| r.job_id == id);
        let mut h = egui::CollapsingHeader::new(RichText::new("Details and log").strong()).id_salt(("det", &id));
        if self.open_details {
            h = h.open(Some(true));
            self.open_details = false;
        }
        h.show(ui, |ui| {
            let lines: Vec<String> = if running_here || !self.live_log.is_empty() {
                self.live_log.iter().cloned().collect()
            } else {
                store::log_tail(&id, 80)
            };
            ui.horizontal(|ui| {
                if ui.small_button("Copy").clicked() {
                    ui.ctx().copy_text(lines.join("\r\n"));
                }
                if ui.small_button("Open log file").clicked() {
                    open_in_explorer(&store::log_path(&id).display().to_string());
                }
                let info = self.tasks.get(&id).cloned().unwrap_or_default();
                if info.exists {
                    ui.label(RichText::new(format!("Windows task: next run {} Â· last run {} (result {}) Â· {}", info.next_run, info.last_run, info.last_result, info.logon_mode)).small().weak())
                        .on_hover_text(&self.cfg.jobs[i].task_name);
                }
            });
            egui::ScrollArea::vertical().max_height(220.0).stick_to_bottom(true).auto_shrink([false, true]).show(ui, |ui| {
                if lines.is_empty() {
                    ui.label(RichText::new("Nothing logged yet.").weak());
                }
                for l in &lines {
                    let t = RichText::new(l).monospace().size(12.0);
                    if l.contains("ERROR") || l.contains("FAILED") {
                        ui.label(t.color(ui.visuals().error_fg_color));
                    } else if l.contains("WARN") {
                        ui.label(t.color(ui.visuals().warn_fg_color));
                    } else {
                        ui.label(t);
                    }
                }
            });
        });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll();
        ui.ctx().request_repaint_after(if self.busy() {
            Duration::from_millis(100)
        } else if !self.bg.is_empty() {
            Duration::from_secs(1)
        } else {
            Duration::from_secs(5)
        });

        egui::Panel::left("jobs").resizable(false).default_size(210.0).show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| self.sidebar(ui));
            ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                ui.label(RichText::new(format!("RoboGUI v{VERSION}")).small().weak());
            });
        });
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                egui::Frame::new().inner_margin(egui::Margin { left: 4, right: 18, top: 0, bottom: 12 }).show(ui, |ui| {
                    ui.set_max_width(ui.available_width().min(760.0));
                    match self.sel {
                        Some(i) if i < self.cfg.jobs.len() => self.job_view(ui, i),
                        _ => self.welcome(ui),
                    }
                });
            });
        });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.save();
    }
}

/// `robogui.exe --run <id>`: run one job without a window (scheduled task).
fn run_headless(id: &str) -> i32 {
    let cfg = store::load_config();
    let Some(job) = cfg.jobs.iter().find(|j| j.id == id) else {
        store::append_log(id, &[format!("{} scheduled run: job {id} no longer exists", now().format("%Y-%m-%d %H:%M:%S"))]);
        return 2;
    };
    let ctl = Control::default();
    match engine::execute(job, true, &ctl, &mut |_| {}) {
        Some(r) if r.ok => 0,
        Some(_) => 1,
        None => 3, // already running
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Some(pos) = args.iter().position(|a| a == "--run") {
        let code = args.get(pos + 1).map(|id| run_headless(id)).unwrap_or(2);
        std::process::exit(code);
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(format!("RoboGUI v{VERSION}"))
            .with_inner_size([980.0, 820.0])
            .with_min_inner_size([720.0, 520.0]),
        ..Default::default()
    };
    let _ = eframe::run_native(
        "RoboGUI",
        options,
        Box::new(|cc| {
            cc.egui_ctx.global_style_mut(|s| {
                s.spacing.item_spacing = egui::vec2(8.0, 7.0);
                s.spacing.button_padding = egui::vec2(10.0, 5.0);
            });
            Ok(Box::new(App::new()))
        }),
    );
}
