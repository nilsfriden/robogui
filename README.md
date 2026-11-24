# RoboGUI v0.3.0

A folder backup tool for Windows (Rust + egui). Each backup job is a folder,
a destination, a method, a schedule and a rule for what to keep. Scheduled
jobs run as Windows scheduled tasks in the background, so RoboGUI doesn't need
to be open. When you open it, it shows what ran and whether it worked.

## Download

Get `robogui-vX.Y.Z.exe` from the [latest release](https://github.com/nilsfriden/robogui/releases/latest).
It's a single file with nothing to install. Windows 10/11, 64-bit.

Put it in a permanent folder before you set up schedules: the scheduled tasks
start the exe from wherever it is.

**Updating:** open the new version. Jobs whose scheduled task still starts the
old exe say "The schedule starts another copy of RoboGUI"; press **Fix** on
each, then you can delete the old exe. RoboGUI never changes scheduled tasks
on its own at startup (antivirus treats that as suspicious, and it should be
your call).

## Build

    cargo build --release

Cargo builds `target\release\robogui.exe`. Releases rename it to
`robogui-v<version>.exe`, with the version from `Cargo.toml`. On start,
RoboGUI shows a Fix button for tasks that start a different exe.

## Methods

- **Dated zip copies**: a new zip each run (`<prefix>_YYYY-MM-DD_HHMM.zip`),
  then the old ones are cleaned up: the newest N are kept, plus the newest
  backup from each of the last N Sundays. Restore unzips into a new folder
  and never overwrites anything.
- **Mirror (exact copy)**: robocopy /MIR. Deletes files from the backup that
  were deleted from the source. No history.
- **Copy, never delete**: robocopy /E. Never deletes anything in the backup.
- **Full drive image (Windows Backup)**: `wbadmin start backup -include:C:
  -allCritical`, a bootable image of a whole drive taken from a snapshot.
  After the first run only changed blocks are stored. Older versions are kept
  as shadow copies on the backup drive, capped at a share of it (Keep, default
  50 %); when that's full Windows deletes the oldest first. Needs admin: open
  RoboGUI as administrator to set it up (there's a button), and the scheduled
  task then runs with highest privileges. Without admin, Back up now starts
  the scheduled task, and the window follows its progress. The list of image
  versions is read after each run. Restore with Windows Recovery > System
  Image Recovery.

Every robocopy option has plain-language hover help in the app.

## Scheduling and reliability

- Each scheduled job is a task in Task Scheduler under `\RoboGUI\`. It runs
  `robogui.exe --run <job id>` with no window.
- The task runs whether you're logged on or not, when Windows allows it.
  Otherwise it falls back to "only when logged on" and the app tells you.
- If the PC was off or asleep at the scheduled time, the backup runs as soon
  as it's back.
- If the backup drive is missing, a scheduled run retries 3 times, 5 minutes
  apart, before it counts as failed.
- On start, the app checks that each task still exists in Windows and starts
  this exe (offering Fix if not), and flags missed runs ("The backup due today at 04:00 didn't run").

## Files

    %APPDATA%\RoboGUI\jobs.json         jobs
    %APPDATA%\RoboGUI\state\<id>.json   run history (last 50 runs)
    %APPDATA%\RoboGUI\logs\<id>.log     log
    %APPDATA%\RoboGUI\state\<id>.progress        progress of a running job
    %APPDATA%\RoboGUI\state\<id>.versions.json   drive image versions

## License

MIT, see [LICENSE](LICENSE).
