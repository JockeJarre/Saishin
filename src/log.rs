use std::io::Write as _;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

static QUIET: AtomicBool = AtomicBool::new(false);
static LAST_WIDTH: AtomicUsize = AtomicUsize::new(0);

pub fn set_quiet(quiet: bool) {
    QUIET.store(quiet, Ordering::Relaxed);
}

pub fn is_quiet() -> bool {
    QUIET.load(Ordering::Relaxed)
}

pub fn info(message: &str) {
    if !is_quiet() {
        println!("{message}");
    }
}

pub fn detail(message: &str) {
    if !is_quiet() {
        println!("    {message}");
    }
}

pub fn progress(message: &str) {
    if !is_quiet() {
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
    if !is_quiet() {
        println!();
    }
    LAST_WIDTH.store(0, Ordering::Relaxed);
}

pub fn warn(message: &str) {
    eprintln!("warning: {message}");
}

pub fn error(message: &str) {
    eprintln!("error: {message}");
}
