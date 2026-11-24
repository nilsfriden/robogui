//! Robocopy options, command-line building and output parsing.
//! Pure logic, no I/O, so it can be unit tested on any platform.

use serde::{Deserialize, Serialize};

/// Robocopy settings for the Mirror / Copy methods.
#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct RoboOpts {
    pub skip_older: bool,
    pub multithread: bool,
    pub threads: u32,
    pub retries: u32,
    pub wait: u32,
    pub restartable: bool,
    pub exclude_junctions: bool,
    pub fat_times: bool,
    pub copy_dir_times: bool,
    pub extra: String,
}

impl Default for RoboOpts {
    fn default() -> Self {
        Self {
            skip_older: false,
            multithread: true,
            threads: 8,
            retries: 3,
            wait: 5,
            restartable: false,
            exclude_junctions: true,
            fat_times: false,
            copy_dir_times: true,
            extra: String::new(),
        }
    }
}

/// Plain-language hover help for every robocopy option the app exposes.
pub mod help {
    pub const MIRROR: &str = "Robocopy /MIR\n\nMakes the destination an exact copy of the source.\nFiles you delete from the source are also deleted from the destination.\nFast after the first run because only changes are copied.\nNo history: if a file gets corrupted, the corrupted version is copied too.";
    pub const COPY: &str = "Robocopy /E\n\nCopies new and changed files, including all subfolders.\nNever deletes anything in the destination, so files you delete from the source stay in the backup.\nThe destination grows over time.";
    pub const ZIP: &str = "Makes a new zip file each time, named with the date and time.\nOld zips are removed according to the Keep setting.\nGives you history you can restore from. Best for small to medium folders, like a Resolve project library.";
    pub const IMAGE: &str = "Windows Backup (wbadmin)\n\nA complete image of a whole drive, including Windows and everything needed to start the PC, made from a snapshot so files in use are included too.\nAfter the first run only changed blocks are saved. Older versions stay on the backup drive until the space limit in Keep is reached; then Windows deletes the oldest first.\nRestore with Windows Recovery > Troubleshoot > System Image Recovery.\nNeeds administrator rights: the schedule runs it with them in the background.";
    pub const SKIP_OLDER: &str = "Robocopy /XO\n\nIf the file in the destination is newer than the one in the source, leave it alone.\nUseful if you sometimes edit files directly in the backup location.";
    pub const DIR_TIMES: &str = "Robocopy /DCOPY:DAT\n\nGives copied folders the same created/modified dates as the originals.\nWithout it, folders get today's date.";
    pub const JUNCTIONS: &str = "Robocopy /XJ\n\nSkips junction points and symbolic links (shortcuts to other folders that Windows puts in places like AppData).\nPrevents endless loops and copying the same data twice. Leave this on unless you know you need links followed.";
    pub const RESTARTABLE: &str = "Robocopy /Z\n\nIf the connection drops halfway through a big file, the copy resumes where it stopped instead of starting over.\nWorth it for network drives and NAS; slows down local copies a little.";
    pub const FAT_TIMES: &str = "Robocopy /FFT\n\nTreats timestamps that are up to 2 seconds apart as equal.\nTurn on when the destination is a NAS, an exFAT/FAT32 drive or anything not NTFS. Otherwise robocopy may think every file changed and copy everything every time.";
    pub const THREADS: &str = "Robocopy /MT:n\n\nCopies several files at the same time.\n8 is a good default. Higher helps with many small files on fast drives or networks; lower (or off) is gentler on a single spinning disk.";
    pub const RETRIES: &str = "Robocopy /R:n\n\nHow many times to retry a file that can't be copied (for example because a program has it locked).\nRobocopy's own default is 1 million, which can make a backup hang for days, so keep this low.";
    pub const WAIT: &str = "Robocopy /W:n\n\nSeconds to wait between retries.\nRobocopy's own default is 30 seconds.";
    pub const EXCLUDE_FILES: &str = "Robocopy /XF (also used for zip copies)\n\nFiles to leave out, separated by semicolons. * and ? work as wildcards.\nExample: *.tmp; Thumbs.db; *.lock";
    pub const EXCLUDE_DIRS: &str = "Robocopy /XD (also used for zip copies)\n\nFolders to leave out, by name, separated by semicolons. Any folder with that name is skipped, wherever it is.\nExample: CacheClip; .gallery; Proxy";
    pub const EXTRA: &str = "Anything here is added to the end of the robocopy command as-is.\nFor options this window doesn't have. Leave empty if unsure.";
}

/// Robocopy (like most Windows tools) treats `\"` as an escaped quote, so
/// `"C:\My Folder\"` breaks the command line. Strip trailing backslashes, and
/// turn a bare drive (`E:`) into `E:\.` which robocopy accepts.
pub fn normalize_path(p: &str) -> String {
    let t = p.trim().trim_matches('"');
    let mut s = t.trim_end_matches(['\\', '/']).to_string();
    if s.len() == 2 && s.ends_with(':') {
        s.push_str("\\.");
    }
    s
}

/// Quote an argument if it contains spaces or is empty.
pub fn quote(arg: &str) -> String {
    if arg.is_empty() || arg.contains([' ', '\t']) {
        format!("\"{arg}\"")
    } else {
        arg.to_string()
    }
}

pub fn split_list(s: &str) -> Vec<String> {
    s.split(';').map(|x| x.trim()).filter(|x| !x.is_empty()).map(String::from).collect()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Purpose {
    /// What the user sees in "Show command".
    Display,
    /// Actual run (adds output flags the progress parser needs).
    Run,
    /// List-only pre-scan to size the job.
    Scan,
}

/// Build the already-quoted argument list (without `robocopy` itself).
pub fn build_args(
    source: &str,
    dest: &str,
    mirror: bool,
    o: &RoboOpts,
    exclude_files: &str,
    exclude_dirs: &str,
    purpose: Purpose,
) -> Vec<String> {
    let mut a = vec![quote(&normalize_path(source)), quote(&normalize_path(dest))];
    a.push(if mirror { "/MIR" } else { "/E" }.into());
    if o.skip_older {
        a.push("/XO".into());
    }
    if o.copy_dir_times {
        a.push("/DCOPY:DAT".into());
    }
    if o.restartable {
        a.push("/Z".into());
    }
    if o.exclude_junctions {
        a.push("/XJ".into());
    }
    if o.fat_times {
        a.push("/FFT".into());
    }
    if o.multithread {
        a.push(format!("/MT:{}", o.threads.clamp(1, 128)));
    }
    a.push(format!("/R:{}", o.retries));
    a.push(format!("/W:{}", o.wait));

    let xf = split_list(exclude_files);
    if !xf.is_empty() {
        a.push("/XF".into());
        a.extend(xf.iter().map(|x| quote(x)));
    }
    let xd = split_list(exclude_dirs);
    if !xd.is_empty() {
        a.push("/XD".into());
        a.extend(xd.iter().map(|x| quote(x)));
    }
    if purpose != Purpose::Display {
        a.extend(["/BYTES", "/FP", "/NP", "/NDL"].map(String::from));
    }
    if purpose == Purpose::Scan {
        a.push("/L".into());
    }
    let extra = o.extra.trim();
    if !extra.is_empty() {
        a.push(extra.to_string());
    }
    a
}

/// One file line from robocopy output (with /BYTES /FP /NDL /NP).
#[derive(Debug, PartialEq, Eq)]
pub struct FileLine {
    pub bytes: u64,
    /// `*EXTRA File` lines: deletions in /MIR, not copies.
    pub extra: bool,
}

/// Recognise a file line. Format (tab separated):
/// `\t    New File  \t\t   10485760\tC:\src\file.bin`
/// Class names are localised on non-English Windows, so we don't match them;
/// we look for a numeric size field followed by a path field. Extras are
/// marked with a leading `*` in every language.
pub fn parse_file_line(line: &str) -> Option<FileLine> {
    let fields: Vec<&str> = line.split('\t').map(str::trim).filter(|f| !f.is_empty()).collect();
    if fields.len() < 2 {
        return None;
    }
    let path = fields[fields.len() - 1];
    let size = fields[fields.len() - 2];
    let looks_like_path = path.len() > 2 && (path.as_bytes()[1] == b':' || path.starts_with("\\\\"));
    if !looks_like_path || size.is_empty() || !size.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let class = if fields.len() >= 3 { fields[0] } else { "" };
    Some(FileLine { bytes: size.parse().ok()?, extra: class.starts_with('*') })
}

/// Robocopy exit codes are a bit field; < 8 means success.
/// Returns (success, plain-language summary).
pub fn describe_exit(code: i32) -> (bool, String) {
    if code < 0 {
        return (false, "Stopped".into());
    }
    if code >= 16 {
        return (false, "Robocopy couldn't run. Check that both folders exist and you have access.".into());
    }
    if code & 8 != 0 {
        return (false, "Some files couldn't be copied. Open Details to see which.".into());
    }
    if code & 1 != 0 {
        return (true, "Changed files copied".into());
    }
    (true, "Already up to date".into())
}

/// Case-insensitive wildcard match (`*` and `?`), as robocopy does for /XF /XD.
pub fn wildcard_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let n: Vec<char> = name.to_lowercase().chars().collect();
    let (mut pi, mut ni, mut star, mut mark) = (0usize, 0usize, None::<usize>, 0usize);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ni;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

pub fn human_bytes(b: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        assert_eq!(normalize_path(r"C:\My Folder\"), r"C:\My Folder");
        assert_eq!(normalize_path(r"E:\"), r"E:\.");
        assert_eq!(quote(r"C:\My Folder"), "\"C:\\My Folder\"");
    }

    #[test]
    fn args() {
        let o = RoboOpts::default();
        let d = build_args(r"C:\Src Dir\", r"E:\", true, &o, "*.tmp; Thumbs.db", "Cache Dir", Purpose::Display).join(" ");
        assert_eq!(d, r#""C:\Src Dir" E:\. /MIR /DCOPY:DAT /XJ /MT:8 /R:3 /W:5 /XF *.tmp Thumbs.db /XD "Cache Dir""#);
        let s = build_args("C:\\a", "D:\\b", false, &o, "", "", Purpose::Scan).join(" ");
        assert!(s.contains("/E ") && s.ends_with("/BYTES /FP /NP /NDL /L"));
    }

    #[test]
    fn file_lines() {
        assert_eq!(
            parse_file_line("\t    New File  \t\t   10485760\tC:\\src\\file.bin"),
            Some(FileLine { bytes: 10485760, extra: false })
        );
        assert_eq!(parse_file_line("\t    Ny fil  \t\t   42\tC:\\src\\åäö.mov"), Some(FileLine { bytes: 42, extra: false }));
        assert_eq!(parse_file_line("\t*EXTRA File \t\t      100\tE:\\dst\\old.txt"), Some(FileLine { bytes: 100, extra: true }));
        assert_eq!(parse_file_line("   Files :        10         2         8"), None);
        assert_eq!(parse_file_line("2026/11/17 04:00:01 ERROR 32 (0x00000020) Copying File C:\\x"), None);
    }

    #[test]
    fn wildcards() {
        assert!(wildcard_match("*.tmp", "a.TMP"));
        assert!(wildcard_match("thumbs.db", "Thumbs.db"));
        assert!(wildcard_match("file?.txt", "file1.txt"));
        assert!(!wildcard_match("*.tmp", "a.tmpx"));
        assert!(wildcard_match("*cache*", "CacheClip"));
        assert!(!wildcard_match("Cache", "CacheClip"));
    }

    #[test]
    fn exits() {
        assert!(describe_exit(0).0);
        assert!(describe_exit(3).0);
        assert!(!describe_exit(9).0);
        assert!(!describe_exit(16).0);
        assert_eq!(human_bytes(1536), "1.5 KB");
    }
}
