//! The backup engine: dated zip copies (with retention), robocopy mirror /
//! copy, restore, and the run wrapper that writes history and logs.
//! Used both by the GUI (in a worker thread) and by `--run` (scheduled).

use crate::robo::{self, human_bytes, wildcard_match, Purpose};
use crate::runner::{run_process, Control};
use crate::store::{self, Job, Method, RunRecord};
use chrono::{Datelike, Local, NaiveDateTime, Timelike};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub enum Event {
    Phase(String),
    Line(String),
    Total { files: u64, bytes: u64 },
    Progress { files: u64, bytes: u64 },
}

pub struct Outcome {
    pub ok: bool,
    pub warnings: u32,
    pub message: String,
    pub size: u64,
}

impl Outcome {
    fn fail(msg: impl Into<String>) -> Self {
        Outcome { ok: false, warnings: 0, message: msg.into(), size: 0 }
    }
}

/// The drive (or share) the destination lives on is reachable.
pub fn dest_drive_available(dest: &str) -> bool {
    let p = Path::new(dest.trim());
    p.ancestors().last().is_some_and(|root| root.exists()) || p.exists()
}

pub fn drive_label(dest: &str) -> String {
    let d = dest.trim();
    if d.len() >= 2 && d.as_bytes()[1] == b':' {
        d[..2].to_uppercase()
    } else {
        d.to_string()
    }
}

// ---------------------------------------------------------------------------
// Zip backups: naming, listing, retention
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Backup {
    pub path: PathBuf,
    pub time: NaiveDateTime,
    pub size: u64,
    pub weekly: bool,
}

pub fn parse_backup_name(prefix: &str, file_name: &str) -> Option<NaiveDateTime> {
    let stem = file_name.strip_suffix(".zip")?;
    let rest = stem.strip_prefix(prefix)?.strip_prefix('_')?;
    NaiveDateTime::parse_from_str(rest, "%Y-%m-%d_%H%M%S")
        .or_else(|_| NaiveDateTime::parse_from_str(&format!("{rest}00"), "%Y-%m-%d_%H%M%S"))
        .ok()
}

/// Which backups to keep: the newest `keep_daily`, plus the newest backup
/// from each of the last `keep_weekly` Sundays. Input newest first.
/// Returns (keep, weekly) flags per index.
pub fn retention_plan(times: &[NaiveDateTime], keep_daily: u32, keep_weekly: u32) -> Vec<(bool, bool)> {
    let mut out = vec![(false, false); times.len()];
    for o in out.iter_mut().take(keep_daily.max(1) as usize) {
        o.0 = true;
    }
    let mut sundays_seen = Vec::new();
    for (i, t) in times.iter().enumerate() {
        if t.weekday() == chrono::Weekday::Sun && !sundays_seen.contains(&t.date()) {
            if sundays_seen.len() as u32 >= keep_weekly {
                break;
            }
            sundays_seen.push(t.date());
            out[i] = (true, true);
        }
    }
    out
}

/// Zip backups in the job's destination, newest first.
pub fn list_backups(job: &Job) -> Vec<Backup> {
    let mut v: Vec<Backup> = std::fs::read_dir(job.dest.trim())
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    let time = parse_backup_name(&job.prefix, &name)?;
                    let size = e.metadata().map(|m| m.len()).unwrap_or(0);
                    Some(Backup { path: e.path(), time, size, weekly: false })
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort_by(|a, b| b.time.cmp(&a.time));
    let times: Vec<NaiveDateTime> = v.iter().map(|b| b.time).collect();
    for (b, (_, weekly)) in v.iter_mut().zip(retention_plan(&times, job.keep_daily, job.keep_weekly)) {
        b.weekly = weekly;
    }
    v
}

fn apply_retention(job: &Job, ev: &mut dyn FnMut(Event)) {
    let backups = list_backups(job);
    let times: Vec<NaiveDateTime> = backups.iter().map(|b| b.time).collect();
    for (b, (keep, _)) in backups.iter().zip(retention_plan(&times, job.keep_daily, job.keep_weekly)) {
        if !keep {
            match std::fs::remove_file(&b.path) {
                Ok(()) => ev(Event::Line(format!("Removed old backup {}", b.path.file_name().unwrap().to_string_lossy()))),
                Err(e) => ev(Event::Line(format!("WARN: couldn't remove {}: {e}", b.path.display()))),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Zip engine
// ---------------------------------------------------------------------------

struct Entry {
    abs: PathBuf,
    rel: String,
    len: u64,
    dir: bool,
}

fn excluded(name: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|p| wildcard_match(p, name))
}

fn collect(dir: &Path, rel: &str, xf: &[String], xd: &[String], out: &mut Vec<Entry>, ev: &mut dyn FnMut(Event), warnings: &mut u32) {
    let rd = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) => {
            *warnings += 1;
            ev(Event::Line(format!("WARN: couldn't read folder {}: {e}", dir.display())));
            return;
        }
    };
    let mut items: Vec<_> = rd.flatten().collect();
    items.sort_by_key(|e| e.file_name());
    for e in items {
        let name = e.file_name().to_string_lossy().to_string();
        let Ok(md) = std::fs::symlink_metadata(e.path()) else { continue };
        if md.file_type().is_symlink() {
            continue; // links and junctions: never followed
        }
        let r = format!("{rel}/{name}");
        if md.is_dir() {
            if excluded(&name, xd) {
                continue;
            }
            out.push(Entry { abs: e.path(), rel: r.clone(), len: 0, dir: true });
            collect(&e.path(), &r, xf, xd, out, ev, warnings);
        } else if md.is_file() && !excluded(&name, xf) {
            out.push(Entry { abs: e.path(), rel: r, len: md.len(), dir: false });
        }
    }
}

fn zip_time(md: &std::fs::Metadata) -> Option<zip::DateTime> {
    let t: chrono::DateTime<Local> = md.modified().ok()?.into();
    zip::DateTime::from_date_and_time(t.year() as u16, t.month() as u8, t.day() as u8, t.hour() as u8, t.minute() as u8, t.second() as u8).ok()
}

/// Open a file, retrying a few times if another program has it locked.
fn open_retry(p: &Path, ctl: &Control) -> std::io::Result<File> {
    let mut last = None;
    for _ in 0..3 {
        match File::open(p) {
            Ok(f) => return Ok(f),
            Err(e) => last = Some(e),
        }
        if ctl.stopped() {
            break;
        }
        std::thread::sleep(Duration::from_millis(1000));
    }
    Err(last.unwrap())
}

fn run_zip(job: &Job, ctl: &Control, ev: &mut dyn FnMut(Event)) -> Outcome {
    let src = PathBuf::from(job.source.trim());
    let dest = PathBuf::from(job.dest.trim());
    if let Err(e) = std::fs::create_dir_all(&dest) {
        return Outcome::fail(format!("Couldn't create the backup folder: {e}"));
    }

    ev(Event::Phase("Counting files…".into()));
    let mut warnings = 0;
    let top = src.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "backup".into());
    let mut entries = vec![Entry { abs: src.clone(), rel: top.clone(), len: 0, dir: true }];
    collect(&src, &top, &robo::split_list(&job.exclude_files), &robo::split_list(&job.exclude_dirs), &mut entries, ev, &mut warnings);
    let total_files = entries.iter().filter(|e| !e.dir).count() as u64;
    let total_bytes: u64 = entries.iter().map(|e| e.len).sum();
    ev(Event::Total { files: total_files, bytes: total_bytes });
    ev(Event::Line(format!("{total_files} files, {} to zip", human_bytes(total_bytes))));

    let now = Local::now().naive_local();
    let mut name = format!("{}_{}.zip", job.prefix, now.format("%Y-%m-%d_%H%M"));
    if dest.join(&name).exists() {
        name = format!("{}_{}.zip", job.prefix, now.format("%Y-%m-%d_%H%M%S"));
    }
    let final_path = dest.join(&name);
    let partial = dest.join(format!("{name}.partial"));

    ev(Event::Phase("Zipping…".into()));
    let result = (|| -> Result<bool, String> {
        let f = File::create(&partial).map_err(|e| format!("Couldn't create {}: {e}", partial.display()))?;
        let mut zw = zip::ZipWriter::new(std::io::BufWriter::with_capacity(1 << 20, f));
        let (mut done_files, mut done_bytes) = (0u64, 0u64);
        let mut buf = vec![0u8; 1 << 20];
        let mut since_report = 0u64;
        for e in &entries {
            if ctl.stopped() {
                return Ok(false);
            }
            let mut opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated)
                .large_file(e.len >= u32::MAX as u64);
            if let Some(t) = std::fs::metadata(&e.abs).ok().as_ref().and_then(zip_time) {
                opts = opts.last_modified_time(t);
            }
            if e.dir {
                zw.add_directory(format!("{}/", e.rel), opts).map_err(|x| x.to_string())?;
                continue;
            }
            let mut file = match open_retry(&e.abs, ctl) {
                Ok(f) => f,
                Err(err) => {
                    warnings += 1;
                    ev(Event::Line(format!("WARN: skipped (couldn't open): {} - {err}", e.abs.display())));
                    continue;
                }
            };
            zw.start_file(e.rel.as_str(), opts).map_err(|x| x.to_string())?;
            let mut failed = false;
            loop {
                match file.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        zw.write_all(&buf[..n]).map_err(|x| format!("Writing the zip failed: {x}"))?;
                        done_bytes += n as u64;
                        since_report += n as u64;
                        if since_report >= 8 << 20 {
                            since_report = 0;
                            ev(Event::Progress { files: done_files, bytes: done_bytes });
                        }
                    }
                    Err(err) => {
                        warnings += 1;
                        ev(Event::Line(format!("WARN: skipped (read error): {} - {err}", e.abs.display())));
                        failed = true;
                        break;
                    }
                }
            }
            if failed {
                zw.abort_file().map_err(|x| x.to_string())?;
            }
            done_files += 1;
            ev(Event::Progress { files: done_files, bytes: done_bytes });
        }
        let mut w = zw.finish().map_err(|x| format!("Finishing the zip failed: {x}"))?;
        w.flush().map_err(|x| x.to_string())?;
        Ok(true)
    })();

    match result {
        Ok(true) => {}
        Ok(false) => {
            let _ = std::fs::remove_file(&partial);
            return Outcome::fail("Stopped");
        }
        Err(e) => {
            let _ = std::fs::remove_file(&partial);
            return Outcome::fail(e);
        }
    }
    if let Err(e) = std::fs::rename(&partial, &final_path) {
        let _ = std::fs::remove_file(&partial);
        return Outcome::fail(format!("Couldn't finish the backup file: {e}"));
    }
    let size = std::fs::metadata(&final_path).map(|m| m.len()).unwrap_or(0);
    ev(Event::Line(format!("Saved {name} ({})", human_bytes(size))));

    ev(Event::Phase("Removing old backups…".into()));
    apply_retention(job, ev);

    let message = if warnings > 0 {
        format!("{warnings} file(s) couldn't be included. Open Details to see which.")
    } else {
        format!("Zip saved, {}", human_bytes(size))
    };
    Outcome { ok: true, warnings, message, size }
}

// ---------------------------------------------------------------------------
// Robocopy engine (Mirror / Copy)
// ---------------------------------------------------------------------------

pub fn robo_args(job: &Job, purpose: Purpose) -> Vec<String> {
    robo::build_args(&job.source, &job.dest, job.method == Method::Mirror, &job.robo, &job.exclude_files, &job.exclude_dirs, purpose)
}

fn run_robocopy(job: &Job, ctl: &Control, ev: &mut dyn FnMut(Event)) -> Outcome {
    ev(Event::Phase("Checking what has changed…".into()));
    let (mut files, mut bytes) = (0u64, 0u64);
    let code = run_process(ctl, "robocopy.exe", &robo_args(job, Purpose::Scan), &mut |l| {
        if let Some(f) = robo::parse_file_line(l) {
            if !f.extra {
                files += 1;
                bytes += f.bytes;
            }
        }
    });
    if code < 0 || code >= 16 {
        return Outcome::fail(robo::describe_exit(code).1);
    }
    ev(Event::Total { files, bytes });

    ev(Event::Phase("Copying…".into()));
    let (mut done_files, mut done_bytes) = (0u64, 0u64);
    let code = run_process(ctl, "robocopy.exe", &robo_args(job, Purpose::Run), &mut |l| {
        if let Some(f) = robo::parse_file_line(l) {
            if !f.extra {
                done_files += 1;
                done_bytes += f.bytes;
                ev(Event::Progress { files: done_files, bytes: done_bytes });
            }
        } else if !l.trim().is_empty() {
            ev(Event::Line(l.to_string()));
        }
    });
    let (ok, text) = robo::describe_exit(code);
    let warnings = if code >= 0 && code & 8 != 0 { 1 } else { 0 };
    let message = if ok && done_files > 0 { format!("{text}: {done_files} files, {}", human_bytes(done_bytes)) } else { text };
    Outcome { ok, warnings, message, size: done_bytes }
}

// ---------------------------------------------------------------------------
// Run wrapper
// ---------------------------------------------------------------------------

/// Run a job, write its history entry and log. Returns None if the job is
/// already running elsewhere.
pub fn execute(job: &Job, scheduled: bool, ctl: &Control, ev: &mut dyn FnMut(Event)) -> Option<RunRecord> {
    let _lock = store::RunLock::acquire(&job.id)?;
    let started = Local::now().naive_local();
    let mut log = vec![format!(
        "=== {} {} run: {} ===",
        started.format("%Y-%m-%d %H:%M:%S"),
        if scheduled { "scheduled" } else { "manual" },
        job.name
    )];

    let outcome = {
        let mut tee = |e: Event| {
            if let Event::Line(l) = &e {
                if log.len() < 3000 {
                    log.push(l.clone());
                }
            }
            ev(e);
        };
        run_inner(job, scheduled, ctl, &mut tee)
    };

    let rec = RunRecord {
        finished: Local::now().naive_local().format("%Y-%m-%d %H:%M:%S").to_string(),
        scheduled,
        ok: outcome.ok,
        warnings: outcome.warnings,
        message: outcome.message.clone(),
        size: outcome.size,
    };
    log.push(format!("Result: {} - {}", if rec.ok { "OK" } else { "FAILED" }, rec.message));
    store::append_log(&job.id, &log);
    store::record_run(&job.id, rec.clone());
    Some(rec)
}

fn run_inner(job: &Job, scheduled: bool, ctl: &Control, ev: &mut dyn FnMut(Event)) -> Outcome {
    if let Some(p) = job.problem() {
        return Outcome::fail(p);
    }
    if !Path::new(job.source.trim()).is_dir() {
        return Outcome::fail(format!("The folder to back up wasn't found: {}", job.source.trim()));
    }
    // A scheduled run waits for a missing backup drive (e.g. a NAS or USB
    // disk that wakes up slowly): 3 more tries, 5 minutes apart.
    let mut tries = 0;
    while !dest_drive_available(&job.dest) {
        if !scheduled || tries >= 3 {
            return Outcome::fail(format!("The backup drive {} isn't available", drive_label(&job.dest)));
        }
        tries += 1;
        ev(Event::Line(format!("Backup drive {} not available, retrying in 5 minutes ({tries}/3)", drive_label(&job.dest))));
        for _ in 0..60 {
            if ctl.stopped() {
                return Outcome::fail("Stopped");
            }
            std::thread::sleep(Duration::from_secs(5));
        }
    }
    match job.method {
        Method::Zip => run_zip(job, ctl, ev),
        Method::Mirror | Method::Copy => run_robocopy(job, ctl, ev),
    }
}

// ---------------------------------------------------------------------------
// Restore
// ---------------------------------------------------------------------------

/// Unzip a backup into a new folder inside `into`. Never overwrites: the
/// folder gets a " (2)" suffix if the name is taken.
pub fn restore(zip_path: &Path, into: &Path, ctl: &Control, ev: &mut dyn FnMut(Event)) -> Result<PathBuf, String> {
    let stem = zip_path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "restore".into());
    let mut out = into.join(&stem);
    let mut n = 2;
    while out.exists() {
        out = into.join(format!("{stem} ({n})"));
        n += 1;
    }
    let f = File::open(zip_path).map_err(|e| format!("Couldn't open the backup: {e}"))?;
    let mut za = zip::ZipArchive::new(std::io::BufReader::new(f)).map_err(|e| format!("The backup file is damaged: {e}"))?;

    let mut total = 0u64;
    for i in 0..za.len() {
        if let Ok(z) = za.by_index(i) {
            total += z.size();
        }
    }
    ev(Event::Total { files: za.len() as u64, bytes: total });
    ev(Event::Phase("Restoring…".into()));
    std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;

    let (mut done, mut bytes) = (0u64, 0u64);
    let mut buf = vec![0u8; 1 << 20];
    for i in 0..za.len() {
        if ctl.stopped() {
            return Err("Stopped".into());
        }
        let mut z = za.by_index(i).map_err(|e| e.to_string())?;
        let Some(rel) = z.enclosed_name() else { continue }; // unsafe path
        let target = out.join(rel);
        if z.is_dir() {
            std::fs::create_dir_all(&target).map_err(|e| e.to_string())?;
        } else {
            if let Some(p) = target.parent() {
                std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
            }
            let mut w = File::create(&target).map_err(|e| format!("Couldn't write {}: {e}", target.display()))?;
            loop {
                let n = z.read(&mut buf).map_err(|e| format!("Reading the backup failed: {e}"))?;
                if n == 0 {
                    break;
                }
                w.write_all(&buf[..n]).map_err(|e| e.to_string())?;
                bytes += n as u64;
            }
        }
        done += 1;
        ev(Event::Progress { files: done, bytes });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap()
    }

    #[test]
    fn names() {
        assert_eq!(parse_backup_name("ResolveDB", "ResolveDB_2026-11-18_0400.zip"), Some(dt("2026-11-18 04:00")));
        assert!(parse_backup_name("ResolveDB", "ResolveDB_2026-11-18_040012.zip").is_some());
        assert_eq!(parse_backup_name("ResolveDB", "ResolveDB_2026-11-18_0400.zip.partial"), None);
        assert_eq!(parse_backup_name("Resolve", "ResolveDB_2026-11-18_0400.zip"), None);
    }

    #[test]
    fn retention() {
        // 30 nightly backups, newest first, ending Wed 2026-11-18
        let times: Vec<NaiveDateTime> = (0..30).map(|i| dt("2026-11-18 04:00") - chrono::Duration::days(i)).collect();
        let plan = retention_plan(&times, 7, 4);
        let kept: Vec<String> = times.iter().zip(&plan).filter(|(_, p)| p.0).map(|(t, _)| t.format("%m-%d").to_string()).collect();
        // 7 newest (12..18) + Sundays 15, 08, 01, 10-25
        assert_eq!(kept, vec!["11-18", "11-17", "11-16", "11-15", "11-14", "11-13", "11-12", "11-08", "11-01", "10-25"]);
        assert!(plan[3].1); // 11-15 is a Sunday kept as weekly
        // never delete everything
        assert!(retention_plan(&times, 0, 0)[0].0);
    }
}

#[cfg(test)]
mod roundtrip {
    use super::*;

    #[test]
    fn zip_then_restore_is_identical() {
        let base = std::env::temp_dir().join(format!("rg_rt_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let src = base.join("Lib");
        std::fs::create_dir_all(src.join("a/åäö")).unwrap();
        std::fs::create_dir_all(src.join("empty")).unwrap();
        let data: Vec<u8> = (0..2_500_000u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
        std::fs::write(src.join("a/åäö/Project.db"), &data).unwrap();
        std::fs::write(src.join("top.txt"), b"hello").unwrap();
        let dest = base.join("dest");
        let job = Job { source: src.display().to_string(), dest: dest.display().to_string(), prefix: "T".into(), ..Default::default() };
        let ctl = Control::default();
        let out = run_zip(&job, &ctl, &mut |_| {});
        assert!(out.ok, "{}", out.message);
        let backups = list_backups(&job);
        assert_eq!(backups.len(), 1);
        let restored = restore(&backups[0].path, &base.join("r"), &ctl, &mut |_| {}).unwrap();
        assert_eq!(std::fs::read(restored.join("Lib/a/åäö/Project.db")).unwrap(), data);
        assert_eq!(std::fs::read(restored.join("Lib/top.txt")).unwrap(), b"hello");
        assert!(restored.join("Lib/empty").is_dir());
        // restoring again never overwrites: gets a new folder
        let again = restore(&backups[0].path, &base.join("r"), &ctl, &mut |_| {}).unwrap();
        assert_ne!(again, restored);
        let _ = std::fs::remove_dir_all(&base);
    }
}
