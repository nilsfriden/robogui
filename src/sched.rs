//! Windows Task Scheduler integration (schtasks.exe + task XML) and the
//! schedule maths used for "next backup" and missed-run detection.

use crate::runner::{decode_console, hidden_command};
use crate::store::{sanitize, Job, Repeat, Schedule};
use chrono::{Datelike, Duration, NaiveDateTime, NaiveTime};

/// Parse "HH:MM" (24 h). Returns normalised "HH:MM".
pub fn parse_time(t: &str) -> Option<String> {
    let (h, m) = t.trim().split_once(':')?;
    let h: u32 = h.trim().parse().ok()?;
    let m: u32 = m.trim().parse().ok()?;
    (h < 24 && m < 60).then(|| format!("{h:02}:{m:02}"))
}

fn sched_time(s: &Schedule) -> Option<NaiveTime> {
    NaiveTime::parse_from_str(&parse_time(&s.time)?, "%H:%M").ok()
}

fn matches_day(s: &Schedule, dt: NaiveDateTime) -> bool {
    match s.repeat {
        Repeat::Daily => true,
        Repeat::Weekly(d) => dt.weekday() == d.to_chrono(),
    }
}

/// Next scheduled start strictly after `now`.
pub fn next_run(s: &Schedule, now: NaiveDateTime) -> Option<NaiveDateTime> {
    let t = sched_time(s)?;
    (0..8)
        .map(|i| (now.date() + Duration::days(i)).and_time(t))
        .find(|dt| *dt > now && matches_day(s, *dt))
}

/// Most recent scheduled start at or before `now`.
pub fn previous_due(s: &Schedule, now: NaiveDateTime) -> Option<NaiveDateTime> {
    let t = sched_time(s)?;
    (0..8)
        .map(|i| (now.date() - Duration::days(i)).and_time(t))
        .find(|dt| *dt <= now && matches_day(s, *dt))
}

/// The run that was due at `due` counts as missed if nothing finished after
/// it within a grace period (runs can take a while, and a PC that was off
/// catches up when it's switched on).
pub fn missed(due: NaiveDateTime, last_finished: Option<NaiveDateTime>, now: NaiveDateTime, enabled_since: Option<NaiveDateTime>) -> bool {
    if now - due < Duration::hours(2) {
        return false;
    }
    if enabled_since.is_some_and(|e| e > due) {
        return false; // schedule was switched on after that run was due
    }
    !last_finished.is_some_and(|f| f >= due)
}

// ---------------------------------------------------------------------------
// Task XML
// ---------------------------------------------------------------------------

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Logon {
    /// Runs whether or not the user is logged on; no stored password.
    /// Local drives only (no network credentials), which is fine for local
    /// and synced folders.
    Background,
    /// Runs only while the user is logged on.
    Interactive,
}

pub fn task_xml(user: &str, s: &Schedule, logon: Logon, command: &str, arguments: &str, description: &str) -> String {
    let time = parse_time(&s.time).unwrap_or_else(|| "04:00".into());
    let schedule = match s.repeat {
        Repeat::Daily => "<ScheduleByDay><DaysInterval>1</DaysInterval></ScheduleByDay>".to_string(),
        Repeat::Weekly(d) => format!(
            "<ScheduleByWeek><DaysOfWeek><{0} /></DaysOfWeek><WeeksInterval>1</WeeksInterval></ScheduleByWeek>",
            d.name()
        ),
    };
    let logon_type = match logon {
        Logon::Background => "S4U",
        Logon::Interactive => "InteractiveToken",
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>{desc}</Description>
  </RegistrationInfo>
  <Triggers>
    <CalendarTrigger>
      <StartBoundary>2025-01-01T{time}:00</StartBoundary>
      <Enabled>true</Enabled>
      {schedule}
    </CalendarTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>{logon_type}</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT12H</ExecutionTimeLimit>
    <Priority>7</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{cmd}</Command>
      <Arguments>{args}</Arguments>
    </Exec>
  </Actions>
</Task>
"#,
        desc = xml_escape(description),
        user = xml_escape(user),
        cmd = xml_escape(command),
        args = xml_escape(arguments),
    )
}

pub fn current_user() -> String {
    let user = std::env::var("USERNAME").unwrap_or_default();
    match std::env::var("USERDOMAIN") {
        Ok(d) if !d.is_empty() => format!("{d}\\{user}"),
        _ => user,
    }
}

fn run_schtasks(args: &[&str]) -> Result<String, String> {
    let out = hidden_command("schtasks.exe")
        .args(args)
        .output()
        .map_err(|e| format!("Couldn't start schtasks: {e}"))?;
    let text = format!("{}{}", decode_console(&out.stdout), decode_console(&out.stderr));
    if out.status.success() {
        Ok(text.trim().to_string())
    } else {
        Err(text.trim().to_string())
    }
}

fn create_with(name: &str, xml: &str) -> Result<(), String> {
    let mut bytes = vec![0xFF, 0xFE]; // UTF-16 LE BOM, matches the XML header
    for u in xml.encode_utf16() {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    let path = std::env::temp_dir().join(format!("robogui_task_{}.xml", std::process::id()));
    std::fs::write(&path, bytes).map_err(|e| format!("Couldn't write task file: {e}"))?;
    let r = run_schtasks(&["/Create", "/TN", name, "/XML", &path.to_string_lossy(), "/F"]);
    let _ = std::fs::remove_file(&path);
    r.map(|_| ())
}

pub fn task_name_for(job: &Job) -> String {
    format!("RoboGUI\\{} [{}]", sanitize(&job.name), job.id)
}

/// Register (or replace) the task for a job. Tries "run whether logged on or
/// not" first and falls back to "only when logged on".
pub fn install(job: &Job) -> Result<(String, Logon), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe = exe.to_string_lossy().to_string();
    let args = format!("--run {}", job.id);
    let desc = format!("RoboGUI backup job \"{}\". Managed by RoboGUI; change it in the app.", job.name);
    let name = task_name_for(job);
    let user = current_user();
    let bg = create_with(&name, &task_xml(&user, &job.schedule, Logon::Background, &exe, &args, &desc));
    match bg {
        Ok(()) => Ok((name, Logon::Background)),
        Err(e1) => match create_with(&name, &task_xml(&user, &job.schedule, Logon::Interactive, &exe, &args, &desc)) {
            Ok(()) => Ok((name, Logon::Interactive)),
            Err(e2) => Err(format!("Windows didn't accept the scheduled task.\n{e1}\n{e2}")),
        },
    }
}

pub fn delete(name: &str) -> Result<(), String> {
    run_schtasks(&["/Delete", "/TN", name, "/F"]).map(|_| ())
}

/// What Task Scheduler itself reports about a task.
#[derive(Clone, Debug, Default)]
pub struct TaskInfo {
    pub exists: bool,
    pub next_run: String,
    pub last_run: String,
    pub last_result: String,
    pub logon_mode: String,
    /// Path of the program the task starts.
    pub command: String,
}

/// Columns of `schtasks /Query /V /FO CSV` are fixed in order regardless of
/// UI language: 0 HostName, 1 TaskName, 2 Next Run Time, 3 Status,
/// 4 Logon Mode, 5 Last Run Time, 6 Last Result, 7 Author, 8 Task To Run.
pub fn parse_query_csv(text: &str) -> Option<TaskInfo> {
    let line = text.lines().find(|l| l.trim_start().starts_with('"'))?;
    let f = parse_csv_line(line);
    if f.len() < 9 {
        return None;
    }
    Some(TaskInfo {
        exists: true,
        next_run: f[2].clone(),
        logon_mode: f[4].clone(),
        last_run: f[5].clone(),
        last_result: f[6].clone(),
        command: f[8].clone(),
    })
}

fn parse_csv_line(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_q = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if in_q && chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            '"' => in_q = !in_q,
            ',' if !in_q => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}

pub fn query(name: &str) -> TaskInfo {
    if name.is_empty() {
        return TaskInfo::default();
    }
    match run_schtasks(&["/Query", "/TN", name, "/V", "/FO", "CSV", "/NH"]) {
        Ok(text) => parse_query_csv(&text).unwrap_or_default(),
        Err(_) => TaskInfo::default(),
    }
}

/// Make Task Scheduler match the job: create/refresh the task when the
/// schedule is on, remove it when off. Updates `job.task_name`.
/// Returns a message for the user when something needs attention.
pub fn sync(job: &mut Job) -> Result<Option<String>, String> {
    let want = job.schedule.enabled && job.is_complete();
    let desired = task_name_for(job);
    if !job.task_name.is_empty() && (!want || job.task_name != desired) {
        let _ = delete(&job.task_name);
        job.task_name.clear();
    }
    if !want {
        return Ok(None);
    }
    let (name, logon) = install(job)?;
    job.task_name = name;
    Ok((logon == Logon::Interactive).then(|| {
        "Scheduled, but Windows only allows it to run while you're logged in. Run RoboGUI once as administrator and switch the schedule off and on to allow it to run when logged out.".to_string()
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Weekday;

    fn dt(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap()
    }

    #[test]
    fn times() {
        assert_eq!(parse_time("4:00").as_deref(), Some("04:00"));
        assert_eq!(parse_time("24:00"), None);
    }

    #[test]
    fn next_and_previous() {
        let s = Schedule { enabled: true, time: "04:00".into(), repeat: Repeat::Daily, enabled_at: String::new() };
        assert_eq!(next_run(&s, dt("2026-11-18 14:00")), Some(dt("2026-11-19 04:00")));
        assert_eq!(next_run(&s, dt("2026-11-18 03:00")), Some(dt("2026-11-18 04:00")));
        assert_eq!(previous_due(&s, dt("2026-11-18 14:00")), Some(dt("2026-11-18 04:00")));
        // 2026-11-22 is a Sunday
        let w = Schedule { enabled: true, time: "04:00".into(), repeat: Repeat::Weekly(Weekday::Sunday), enabled_at: String::new() };
        assert_eq!(next_run(&w, dt("2026-11-18 14:00")), Some(dt("2026-11-22 04:00")));
        assert_eq!(previous_due(&w, dt("2026-11-18 14:00")), Some(dt("2026-11-15 04:00")));
    }

    #[test]
    fn missed_runs() {
        let due = dt("2026-11-18 04:00");
        let now = dt("2026-11-18 14:00");
        assert!(missed(due, Some(dt("2026-11-17 04:01")), now, None));
        assert!(!missed(due, Some(dt("2026-11-18 04:02")), now, None));
        assert!(!missed(due, None, dt("2026-11-18 05:00"), None)); // grace period
        assert!(!missed(due, None, now, Some(dt("2026-11-18 10:00")))); // enabled after due
        assert!(missed(due, None, now, Some(dt("2026-11-17 10:00"))));
    }

    #[test]
    fn xml() {
        let s = Schedule { enabled: true, time: "4:00".into(), repeat: Repeat::Weekly(Weekday::Sunday), enabled_at: String::new() };
        let x = task_xml(r"PC\Nils", &s, Logon::Background, r"C:\dev\robogui.exe", "--run ab12 & x", "d");
        assert!(x.contains("<StartBoundary>2025-01-01T04:00:00</StartBoundary>"));
        assert!(x.contains("<Sunday />") && x.contains("<LogonType>S4U</LogonType>"));
        assert!(x.contains("--run ab12 &amp; x"));
    }

    #[test]
    fn csv() {
        let t = "\"NILS-PC\",\"\\RoboGUI\\Resolve [ab]\",\"2026-11-19 04:00:00\",\"Klar\",\"Endast bakgrund\",\"2026-11-18 04:00:01\",\"0\",\"Nils\",\"C:\\dev\\robogui.exe --run ab\"";
        let s = parse_query_csv(t).unwrap();
        assert_eq!(s.next_run, "2026-11-19 04:00:00");
        assert_eq!(s.last_result, "0");
        assert!(s.command.contains("robogui.exe"));
    }
}
