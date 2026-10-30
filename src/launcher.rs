// The icon's own program: a launcher that starts the real binary and reports
// what it printed, without ever opening a console window.
//
// Why this file exists. saishin-cli is a console program, and an icon launch of
// a console program gets a console: on a machine whose console host is Windows
// Terminal that is a terminal window, titled with the exe path, visible for a
// few hundred milliseconds and gone with the process. It cannot be fixed from
// inside the process: `ShowWindow(GetConsoleWindow(), SW_HIDE)` hides our own
// invisible pseudoconsole, not the window the host owns. What stops the window
// is the PE subsystem, and the subsystem is a property of the exe, so it needs
// an exe of its own.
//
//   saishin.exe      this file: GUI subsystem, so Windows starts it with no
//                    console at all, carries the icon, and is what a shortcut,
//                    Explorer and the taskbar show.
//   saishin-cli.exe  the program: console subsystem, so a terminal run prints,
//                    the shell waits for it, and its exit code arrives as
//                    always. Scripts and scheduled tasks call this one.
//
// What the launcher does with a run:
//
//   - The program is started with `CREATE_NO_WINDOW` and piped handles, so no
//     console window exists for either process.
//   - Its text is captured, and the box is the report: the console that would
//     have carried it does not exist. `SAISHIN_LAUNCHER` tells the program two
//     things: that it keeps its own box in its pocket, so a failure is not
//     reported twice, and that a dialog is no place to read a run's transcript,
//     so the program prints its warnings and errors and keeps the chatter to
//     itself.
//   - A run with nothing wrong says nothing and exits 0, so it gets no box, and
//     a run that opens the GUI window prints nothing at all: its window, and the
//     log strip in it, is the report.
//   - The exit code is passed through, so a launcher in front of the program
//     changes nothing for whoever watches the code.
//
// Only std: this binary links no tokio, no eframe, no reqwest, which is why it
// starts in milliseconds. The one window call it needs is declared by hand, the
// same way src/notifier.rs declares the box it draws.

// No console window, ever: this is the PE subsystem flag that stops Windows
// from creating one, which is the whole reason the binary exists.
#![cfg_attr(windows, windows_subsystem = "windows")]

/// The name of the program this launcher starts, in its own directory.
const PROGRAM: &str = "saishin-cli";

/// Where the box goes when the box is the only report there is.
#[cfg(windows)]
const CAPTION: &str = "Saishin";

/// `CREATE_NO_WINDOW`: start the child with a console it cannot see. Combined
/// with piped handles, no terminal window is created for the child either.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Longest report a box carries. A whole run's log is not something to read in
/// a dialog, and the tail is where the problem is, so the tail is what stays.
#[cfg(windows)]
const MAX_REPORT: usize = 4000;

#[cfg(windows)]
fn main() {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};

    let args: Vec<String> = std::env::args().skip(1).collect();
    let program = program_path();

    let child = Command::new(&program)
        .args(&args)
        // The program reads its own box rule from this: the report is ours.
        .env("SAISHIN_LAUNCHER", "1")
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();

    let mut child = match child {
        Ok(child) => child,
        Err(ex) => {
            box_up(&format!("Could not start {}:\n{ex}", program.display()), false);
            std::process::exit(1);
        }
    };

    // Both pipes are drained at the same time: a pipe that fills up would stop
    // the run for good, and the point of the launcher is that nothing is lost.
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out = std::thread::spawn(move || read_all(stdout));
    let err = std::thread::spawn(move || read_all(stderr));
    let out = out.join().unwrap_or_default();
    let err = err.join().unwrap_or_default();

    let code = match child.wait() {
        Ok(status) => status.code().unwrap_or(1),
        Err(_) => 1,
    };

    // The program's own words are the report: stderr first, because that is
    // where the problems are, then a headless run's summary on stdout. A run
    // that failed without saying anything says so with its exit code.
    let text = if err.trim().is_empty() { out } else { err };
    let text = trimmed(&text);
    if !text.is_empty() || code != 0 {
        let text = if text.is_empty() {
            format!("Saishin stopped with exit code {code}.")
        } else {
            text
        };
        box_up(&text, code == 0);
    }
    std::process::exit(code);
}

#[cfg(not(windows))]
fn main() {
    // Built to keep `cargo build` whole on other platforms, never shipped:
    // there, the program is the one called saishin.
    eprintln!("This is the Windows launcher. The program is saishin-cli.");
    std::process::exit(2);
}

/// The program beside this launcher, not the one in the terminal's PATH.
#[cfg(windows)]
fn program_path() -> std::path::PathBuf {
    let this = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from(PROGRAM));
    this.parent()
        .map(|dir| dir.join(format!("{PROGRAM}.exe")))
        .unwrap_or_else(|| std::path::PathBuf::from(format!("{PROGRAM}.exe")))
}

/// A pipe read to the end. Both pipes are read on their own threads, so a full
/// pipe cannot stop the run; what comes back is text, lossy on stray bytes.
#[cfg(windows)]
fn read_all<R: std::io::Read>(handle: Option<R>) -> String {
    let Some(mut handle) = handle else {
        return String::new();
    };
    let mut bytes = Vec::new();
    if handle.read_to_end(&mut bytes).is_err() {
        return String::new();
    }
    String::from_utf8_lossy(&bytes).to_string()
}

/// The last MAX_REPORT characters of a report, with a leading ellipsis when
/// there was more to show.
#[cfg(windows)]
fn trimmed(text: &str) -> String {
    let text = text.trim();
    let count = text.chars().count();
    if count <= MAX_REPORT {
        return text.to_string();
    }
    let tail: String = text.chars().skip(count - MAX_REPORT).collect();
    format!("…\n{tail}")
}

/// One modal box drawn by the system: no child process, and this process stays
/// alive while the box is up, so the text can be read.
#[cfg(windows)]
fn box_up(text: &str, went_well: bool) {
    // User32 is linked by name: this binary has no windowing dependency, and
    // one declared link is all a single call needs.
    #[link(name = "user32")]
    extern "system" {
        fn MessageBoxW(
            hwnd: *const core::ffi::c_void,
            text: *const u16,
            caption: *const u16,
            kind: u32,
        ) -> i32;
    }
    const MB_ICONERROR: u32 = 0x0000_0010;
    const MB_ICONINFORMATION: u32 = 0x0000_0040;

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    let caption = wide(CAPTION);
    let text = wide(text);
    let kind = if went_well {
        MB_ICONINFORMATION
    } else {
        MB_ICONERROR
    };
    // Safety: both strings are NUL-terminated and outlive the call.
    unsafe { MessageBoxW(core::ptr::null(), text.as_ptr(), caption.as_ptr(), kind) };
}
