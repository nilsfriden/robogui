//! Helpers for the "Full drive image" method, which uses Windows' own
//! wbadmin: admin checks, relaunching as admin, reading wbadmin output and
//! free space on the backup drive.

use chrono::{Local, NaiveDateTime, TimeZone, Utc};

/// The wbadmin command line for a job (also shown in the window).
pub fn wbadmin_args(source_drive: &str, dest_drive: &str) -> Vec<String> {
    vec![
        "start".into(),
        "backup".into(),
        format!("-backupTarget:{dest_drive}"),
        format!("-include:{source_drive}"),
        "-allCritical".into(),
        "-quiet".into(),
    ]
}

/// vssadmin arguments that cap the space older versions may use on the
/// backup drive. `verb` is "Resize" or "Add" (Add when none is set yet).
pub fn shadow_args(verb: &str, dest_drive: &str, pct: u32) -> Vec<String> {
    vec![
        verb.into(),
        "ShadowStorage".into(),
        format!("/For={dest_drive}"),
        format!("/On={dest_drive}"),
        format!("/MaxSize={}%", pct.clamp(5, 95)),
    ]
}

/// "Creating a backup of volume Windows (C:), copied (45%)." -> Some(45)
pub fn parse_percent(line: &str) -> Option<u64> {
    let end = line.rfind("%)")?;
    let start = line[..end].rfind('(')? + 1;
    line[start..end].trim().parse().ok().filter(|p| *p <= 100)
}

/// The part of a progress line before ", copied (45%)", for the phase text.
pub fn progress_phase(line: &str) -> String {
    let l = line.trim();
    let cut = l.rfind(", ").filter(|_| parse_percent(l).is_some()).map(|i| &l[..i]).unwrap_or(l);
    cut.trim_end_matches('.').to_string()
}

/// Version identifiers in `wbadmin get versions` output look like
/// "11/24/2026-02:00" (UTC, same format in every UI language). Returns the
/// backup times in local time, newest first.
pub fn parse_versions(text: &str) -> Vec<NaiveDateTime> {
    let mut out = Vec::new();
    for word in text.split_whitespace() {
        let w = word.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '/' && c != ':' && c != '-');
        if w.len() != 16 || w.as_bytes()[10] != b'-' {
            continue;
        }
        if let Ok(utc) = NaiveDateTime::parse_from_str(w, "%m/%d/%Y-%H:%M") {
            let local = Utc.from_utc_datetime(&utc).with_timezone(&Local).naive_local();
            if !out.contains(&local) {
                out.push(local);
            }
        }
    }
    out.sort_by(|a, b| b.cmp(a));
    out
}

/// Is this process running with administrator rights?
#[cfg(windows)]
pub fn is_elevated() -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut el = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            &mut el as *mut _ as *mut core::ffi::c_void,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        );
        CloseHandle(token);
        ok != 0 && el.TokenIsElevated != 0
    }
}

#[cfg(not(windows))]
pub fn is_elevated() -> bool {
    false
}

/// Start this program again with administrator rights (Windows asks the user
/// first), with `args` ("" for the normal window). Returns true if it started.
#[cfg(windows)]
pub fn relaunch_as_admin(args: &str) -> bool {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let Ok(exe) = std::env::current_exe() else { return false };
    let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let verb = wide("runas");
    let file = wide(&exe.to_string_lossy());
    let params = wide(args);
    let p = if args.is_empty() { std::ptr::null() } else { params.as_ptr() };
    let r = unsafe { ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), file.as_ptr(), p, std::ptr::null(), SW_SHOWNORMAL) };
    r as usize > 32
}

#[cfg(not(windows))]
pub fn relaunch_as_admin(_args: &str) -> bool {
    false
}

/// (free, total) bytes on the drive that holds `path`.
#[cfg(windows)]
pub fn disk_space(path: &str) -> Option<(u64, u64)> {
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let root = format!("{}\\", crate::store::drive_of(path)?);
    let w: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();
    let (mut free, mut total, mut _all_free) = (0u64, 0u64, 0u64);
    let ok = unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut free, &mut total, &mut _all_free) };
    (ok != 0).then_some((free, total))
}

#[cfg(not(windows))]
pub fn disk_space(_path: &str) -> Option<(u64, u64)> {
    None
}

/// Total size of the files in a folder (0 if it can't be read).
pub fn folder_size(p: &std::path::Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(p) else { return 0 };
    rd.flatten()
        .map(|e| match std::fs::symlink_metadata(e.path()) {
            Ok(m) if m.is_dir() => folder_size(&e.path()),
            Ok(m) if m.is_file() => m.len(),
            _ => 0,
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_lines() {
        let l = "Creating a backup of volume Windows (C:), copied (45%).";
        assert_eq!(parse_percent(l), Some(45));
        assert_eq!(progress_phase(l), "Creating a backup of volume Windows (C:)");
        assert_eq!(parse_percent("Creating a shadow copy of the volumes specified for backup..."), None);
        assert_eq!(progress_phase("Creating a shadow copy..."), "Creating a shadow copy");
    }

    #[test]
    fn versions() {
        let text = "Backup time: 11/23/2026 3:00 AM\r\nBackup target: Fixed Disk labeled Backup(H:)\r\nVersion identifier: 11/23/2026-02:00\r\nCan recover: Volume(s), File(s)\r\n\r\nBackup time: 11/24/2026 3:00 AM\r\nVersion identifier: 11/24/2026-02:00\r\n";
        let v = parse_versions(text);
        assert_eq!(v.len(), 2);
        assert!(v[0] > v[1]);
    }

    #[test]
    fn args() {
        assert_eq!(wbadmin_args("C:", "H:").join(" "), "start backup -backupTarget:H: -include:C: -allCritical -quiet");
        assert_eq!(shadow_args("Resize", "H:", 50).join(" "), "Resize ShadowStorage /For=H: /On=H: /MaxSize=50%");
    }
}
