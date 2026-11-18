//! Running console programs (robocopy, schtasks) without a console window,
//! with stop support and OEM code page decoding.

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Shared between the worker and the UI so the UI can stop a run.
#[derive(Clone, Default)]
pub struct Control {
    child: Arc<Mutex<Option<Child>>>,
    stop: Arc<AtomicBool>,
}

impl Control {
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(c) = self.child.lock().unwrap().as_mut() {
            let _ = c.kill();
        }
    }
    pub fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }
}

/// A Command that won't open a console window.
pub fn hidden_command(program: &str) -> Command {
    #[allow(unused_mut)]
    let mut c = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    c
}

/// Arguments are pre-quoted (so "Show command" matches exactly what runs);
/// pass them through untouched on Windows.
fn add_raw_args(c: &mut Command, args: &[String]) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        for a in args {
            c.raw_arg(a);
        }
    }
    #[cfg(not(windows))]
    {
        c.args(args.iter().map(|a| a.trim_matches('"')));
    }
}

/// Console programs write in the OEM code page (850 on Swedish Windows).
#[cfg(windows)]
pub fn decode_console(bytes: &[u8]) -> String {
    use windows_sys::Win32::Globalization::{MultiByteToWideChar, CP_OEMCP};
    if bytes.is_ascii() {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    unsafe {
        let n = MultiByteToWideChar(CP_OEMCP, 0, bytes.as_ptr(), bytes.len() as i32, std::ptr::null_mut(), 0);
        if n <= 0 {
            return String::from_utf8_lossy(bytes).into_owned();
        }
        let mut buf = vec![0u16; n as usize];
        MultiByteToWideChar(CP_OEMCP, 0, bytes.as_ptr(), bytes.len() as i32, buf.as_mut_ptr(), n);
        String::from_utf16_lossy(&buf)
    }
}

#[cfg(not(windows))]
pub fn decode_console(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Run a program to completion, calling `on_line` for every output line
/// (stdout and stderr). Returns the exit code, or -1 if stopped or it
/// couldn't start.
pub fn run_process(ctl: &Control, program: &str, args: &[String], on_line: &mut dyn FnMut(&str)) -> i32 {
    let mut cmd = hidden_command(program);
    add_raw_args(&mut cmd, args);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(Stdio::null());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            on_line(&format!("ERROR: couldn't start {program}: {e}"));
            return -1;
        }
    };
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    *ctl.child.lock().unwrap() = Some(child);

    // stderr on its own thread so neither pipe can block the other.
    let err_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        decode_console(&buf)
    });

    let mut pending: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = match stdout.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        for &b in &chunk[..n] {
            if b == b'\n' || b == b'\r' {
                if !pending.is_empty() {
                    on_line(&decode_console(&pending));
                    pending.clear();
                }
            } else {
                pending.push(b);
            }
        }
    }
    if !pending.is_empty() {
        on_line(&decode_console(&pending));
    }
    if let Ok(err) = err_thread.join() {
        for l in err.lines().filter(|l| !l.trim().is_empty()) {
            on_line(&format!("ERROR: {l}"));
        }
    }

    let code = match ctl.child.lock().unwrap().take() {
        Some(mut c) => c.wait().ok().and_then(|s| s.code()).unwrap_or(-1),
        None => -1,
    };
    if ctl.stopped() {
        -1
    } else {
        code
    }
}
