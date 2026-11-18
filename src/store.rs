//! Backup jobs, settings file and per-job status, shared by the GUI and the
//! headless `--run` mode the scheduled tasks use.
//!
//! %APPDATA%\RoboGUI\jobs.json          the jobs
//! %APPDATA%\RoboGUI\state\<id>.json    result of the last run
//! %APPDATA%\RoboGUI\logs\<id>.log      run log (trimmed)

use crate::robo::RoboOpts;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub enum Method {
    Zip,
    Mirror,
    Copy,
}

impl Method {
    pub fn label(self) -> &'static str {
        match self {
            Method::Zip => "Dated zip copies",
            Method::Mirror => "Mirror (exact copy)",
            Method::Copy => "Copy, never delete",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub enum Repeat {
    Daily,
    Weekly(Weekday),
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub enum Weekday {
    Monday,
    Tuesday,
    Wednesday,
    Thursday,
    Friday,
    Saturday,
    Sunday,
}

impl Weekday {
    pub const ALL: [Weekday; 7] = [
        Weekday::Monday,
        Weekday::Tuesday,
        Weekday::Wednesday,
        Weekday::Thursday,
        Weekday::Friday,
        Weekday::Saturday,
        Weekday::Sunday,
    ];
    pub fn name(self) -> &'static str {
        match self {
            Weekday::Monday => "Monday",
            Weekday::Tuesday => "Tuesday",
            Weekday::Wednesday => "Wednesday",
            Weekday::Thursday => "Thursday",
            Weekday::Friday => "Friday",
            Weekday::Saturday => "Saturday",
            Weekday::Sunday => "Sunday",
        }
    }
    pub fn to_chrono(self) -> chrono::Weekday {
        use chrono::Weekday as W;
        match self {
            Weekday::Monday => W::Mon,
            Weekday::Tuesday => W::Tue,
            Weekday::Wednesday => W::Wed,
            Weekday::Thursday => W::Thu,
            Weekday::Friday => W::Fri,
            Weekday::Saturday => W::Sat,
            Weekday::Sunday => W::Sun,
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct Schedule {
    pub enabled: bool,
    pub time: String,
    pub repeat: Repeat,
    /// When automatic backup was last switched on (for missed-run checks).
    pub enabled_at: String,
}

impl Default for Schedule {
    fn default() -> Self {
        Self { enabled: false, time: "04:00".into(), repeat: Repeat::Daily, enabled_at: String::new() }
    }
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct Job {
    pub id: String,
    pub name: String,
    pub source: String,
    pub dest: String,
    pub method: Method,
    /// Zip file names: <prefix>_YYYY-MM-DD_HHMM.zip. Fixed at creation so
    /// renaming the job doesn't orphan old backups.
    pub prefix: String,
    pub keep_daily: u32,
    pub keep_weekly: u32,
    pub exclude_files: String,
    pub exclude_dirs: String,
    pub robo: RoboOpts,
    pub schedule: Schedule,
    /// Name of the Task Scheduler task currently registered for this job.
    pub task_name: String,
}

impl Default for Job {
    fn default() -> Self {
        Self {
            id: new_id(),
            name: "New backup".into(),
            source: String::new(),
            dest: String::new(),
            method: Method::Zip,
            prefix: "Backup".into(),
            keep_daily: 7,
            keep_weekly: 4,
            exclude_files: String::new(),
            exclude_dirs: String::new(),
            robo: RoboOpts::default(),
            schedule: Schedule::default(),
            task_name: String::new(),
        }
    }
}

impl Job {
    pub fn is_complete(&self) -> bool {
        !self.source.trim().is_empty() && !self.dest.trim().is_empty()
    }

    /// Problems that stop a run, in plain words.
    pub fn problem(&self) -> Option<String> {
        let s = self.source.trim().trim_end_matches(['\\', '/']).to_lowercase();
        let d = self.dest.trim().trim_end_matches(['\\', '/']).to_lowercase();
        if s.is_empty() {
            return Some("Choose a folder to back up".into());
        }
        if d.is_empty() {
            return Some("Choose where to save the backup".into());
        }
        if s == d {
            return Some("The backup can't go into the folder it's backing up".into());
        }
        if d.starts_with(&format!("{s}\\")) || d.starts_with(&format!("{s}/")) {
            return Some("The backup location is inside the folder being backed up. Pick a folder outside it.".into());
        }
        None
    }
}

pub fn new_id() -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:08x}", (n ^ (n >> 32)) as u32)
}

/// Turn a job name into something safe for file and task names.
pub fn sanitize(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let s = s.trim_matches('_').to_string();
    if s.is_empty() {
        "Backup".into()
    } else {
        s
    }
}

#[derive(Clone, Serialize, Deserialize, Debug, Default)]
#[serde(default)]
pub struct Config {
    pub jobs: Vec<Job>,
    pub selected: Option<String>,
}

#[derive(Clone, Serialize, Deserialize, Debug, Default, PartialEq)]
#[serde(default)]
pub struct RunRecord {
    /// Local time the run finished, "YYYY-MM-DD HH:MM:SS".
    pub finished: String,
    /// true = started by Task Scheduler, false = "Back up now".
    pub scheduled: bool,
    pub ok: bool,
    pub warnings: u32,
    pub message: String,
    pub size: u64,
}

impl RunRecord {
    pub fn finished_time(&self) -> Option<chrono::NaiveDateTime> {
        chrono::NaiveDateTime::parse_from_str(&self.finished, "%Y-%m-%d %H:%M:%S").ok()
    }
}

/// Run history of one job, newest first.
#[derive(Clone, Serialize, Deserialize, Debug, Default)]
#[serde(default)]
pub struct State {
    pub history: Vec<RunRecord>,
}

impl State {
    pub const MAX_HISTORY: usize = 50;
    pub fn last(&self) -> Option<&RunRecord> {
        self.history.first()
    }
    pub fn last_scheduled(&self) -> Option<&RunRecord> {
        self.history.iter().find(|r| r.scheduled)
    }
    pub fn push(&mut self, r: RunRecord) {
        self.history.insert(0, r);
        self.history.truncate(Self::MAX_HISTORY);
    }
}

// ---------------------------------------------------------------------------

pub fn app_dir() -> PathBuf {
    // Scheduled tasks running while logged out may lack APPDATA.
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(|h| PathBuf::from(h).join("AppData").join("Roaming")))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("RoboGUI")
}

fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)
}

pub fn load_config() -> Config {
    std::fs::read_to_string(app_dir().join("jobs.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_config(c: &Config) -> std::io::Result<()> {
    let s = serde_json::to_string_pretty(c).map_err(std::io::Error::other)?;
    write_atomic(&app_dir().join("jobs.json"), s.as_bytes())
}

pub fn load_state(id: &str) -> State {
    std::fs::read_to_string(app_dir().join("state").join(format!("{id}.json")))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Add a run to the job's history (re-reads first, so concurrent writers
/// don't drop each other's entries).
pub fn record_run(id: &str, r: RunRecord) {
    let mut s = load_state(id);
    s.push(r);
    save_state(id, &s);
}

pub fn save_state(id: &str, s: &State) {
    if let Ok(j) = serde_json::to_string_pretty(s) {
        let _ = write_atomic(&app_dir().join("state").join(format!("{id}.json")), j.as_bytes());
    }
}

pub fn log_path(id: &str) -> PathBuf {
    app_dir().join("logs").join(format!("{id}.log"))
}

pub fn append_log(id: &str, lines: &[String]) {
    use std::io::Write;
    let p = log_path(id);
    let _ = std::fs::create_dir_all(p.parent().unwrap());
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        for l in lines {
            let _ = writeln!(f, "{l}");
        }
    }
    // Keep the last 2000 lines.
    if std::fs::metadata(&p).map(|m| m.len() > 400_000).unwrap_or(false) {
        if let Ok(s) = std::fs::read_to_string(&p) {
            let v: Vec<&str> = s.lines().collect();
            let keep = v[v.len().saturating_sub(2000)..].join("\n");
            let _ = std::fs::write(&p, keep + "\n");
        }
    }
}

pub fn log_tail(id: &str, n: usize) -> Vec<String> {
    std::fs::read_to_string(log_path(id))
        .map(|s| {
            let v: Vec<String> = s.lines().map(String::from).collect();
            v[v.len().saturating_sub(n)..].to_vec()
        })
        .unwrap_or_default()
}

/// One run per job at a time (GUI and scheduled task may overlap).
pub struct RunLock(PathBuf);

impl RunLock {
    pub fn acquire(id: &str) -> Option<RunLock> {
        let p = app_dir().join("state").join(format!("{id}.lock"));
        let _ = std::fs::create_dir_all(p.parent().unwrap());
        // A lock older than 12 hours is from a crashed run.
        if let Ok(m) = std::fs::metadata(&p) {
            let stale = m.modified().ok().and_then(|t| t.elapsed().ok()).is_some_and(|e| e.as_secs() > 12 * 3600);
            if stale {
                let _ = std::fs::remove_file(&p);
            }
        }
        std::fs::OpenOptions::new().write(true).create_new(true).open(&p).ok().map(|_| RunLock(p))
    }
}

impl Drop for RunLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Where DaVinci Resolve keeps its local project library, if it can be found.
pub fn find_resolve_library() -> Option<PathBuf> {
    let mut c = Vec::new();
    for var in ["APPDATA", "PROGRAMDATA"] {
        if let Some(base) = std::env::var_os(var) {
            let s = PathBuf::from(base).join("Blackmagic Design").join("DaVinci Resolve").join("Support");
            c.push(s.join("Resolve Project Library"));
            c.push(s.join("Resolve Disk Database"));
        }
    }
    c.into_iter().find(|p| p.is_dir())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn problems() {
        let mut j = Job { source: r"C:\a".into(), dest: r"C:\a\b".into(), ..Default::default() };
        assert!(j.problem().is_some());
        j.dest = r"C:\ab".into();
        assert!(j.problem().is_none());
        j.dest = r"c:\A\".into();
        assert!(j.problem().is_some());
    }

    #[test]
    fn names() {
        assert_eq!(sanitize("Resolve library!"), "Resolve_library");
        assert_eq!(sanitize("  "), "Backup");
    }

    #[test]
    fn config_roundtrip_with_missing_fields() {
        let c: Config = serde_json::from_str(r#"{"jobs":[{"id":"x","name":"A"}]}"#).unwrap();
        assert_eq!(c.jobs[0].keep_daily, 7);
        assert_eq!(c.jobs[0].method, Method::Zip);
    }
}
