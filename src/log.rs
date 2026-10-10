use std::sync::atomic::{AtomicBool, Ordering};

static QUIET: AtomicBool = AtomicBool::new(false);

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

pub fn warn(message: &str) {
    eprintln!("warning: {message}");
}

pub fn error(message: &str) {
    eprintln!("error: {message}");
}
