use std::io::Write as _;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

static QUIET: AtomicBool = AtomicBool::new(false);
static LAST_WIDTH: AtomicUsize = AtomicUsize::new(0);

/// Off when a window is the report. The GUI switches this before it gives back
/// the console that launching the exe from a shortcut created: printing to that
/// console is what keeps a command window standing next to the window.
static CONSOLE: AtomicBool = AtomicBool::new(true);

/// Off in a run that opens the window: a stream of progress and information has
/// no reader there, the window is the report. Warnings and errors keep the
/// console rule below, because a failure before the window exists is text
/// somebody needs to be shown.
static STREAM: AtomicBool = AtomicBool::new(true);

pub fn set_quiet(quiet: bool) {
    QUIET.store(quiet, Ordering::Relaxed);
}

pub fn is_quiet() -> bool {
    QUIET.load(Ordering::Relaxed)
}

pub fn set_console_output(on: bool) {
    CONSOLE.store(on, Ordering::Relaxed);
}

pub fn is_console_output() -> bool {
    CONSOLE.load(Ordering::Relaxed)
}

/// Call this as soon as a run has decided to open the window. Before the token
/// is looked up, before the first source is asked: a run in front of a launcher
/// would otherwise show `GitHub token taken from the secure store.` in a box,
/// the one line printed while the window is the report.
pub fn window_is_the_report() {
    STREAM.store(false, Ordering::Relaxed);
}

fn is_stream() -> bool {
    STREAM.load(Ordering::Relaxed)
}

fn on_console() -> bool {
    !is_quiet() && is_console_output() && is_stream()
}

pub fn info(message: &str) {
    if on_console() {
        println!("{message}");
    }
}

pub fn detail(message: &str) {
    if on_console() {
        println!("    {message}");
    }
}

pub fn progress(message: &str) {
    if on_console() {
        let line = format!("    {message}");
        let len = line.len();
        let prev = LAST_WIDTH.load(Ordering::Relaxed);
        let width = len.max(prev);
        let padded = if len < width {
            format!("{line}{}", " ".repeat(width - len))
        } else {
            line
        };
        print!("\r{padded}");
        let _ = std::io::stdout().flush();
        LAST_WIDTH.store(len, Ordering::Relaxed);
    }
}

pub fn progress_done() {
    if on_console() {
        println!();
    }
    LAST_WIDTH.store(0, Ordering::Relaxed);
}

pub fn warn(message: &str) {
    if is_console_output() {
        eprintln!("warning: {message}");
    }
}

pub fn error(message: &str) {
    if is_console_output() {
        eprintln!("error: {message}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_front_end_can_take_the_report_off_the_console() {
        assert!(is_console_output());
        set_console_output(false);
        assert!(!is_console_output());
        set_console_output(true);
    }

    #[test]
    fn a_window_takes_the_stream_and_leaves_the_problems() {
        window_is_the_report();
        // info, detail and progress are off ...
        assert!(!is_stream());
        // ... and warn and error are not affected by that switch.
        assert!(is_console_output());
        STREAM.store(true, Ordering::Relaxed);
    }
}
