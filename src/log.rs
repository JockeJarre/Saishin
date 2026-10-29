use std::io::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;

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

/// Which mouth a line came out of, so a window can colour the problems and the
/// run's own chatter alike.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    Info,
    Detail,
    Progress,
    Warn,
    Error,
}

#[derive(Clone)]
pub struct Line {
    pub level: Level,
    pub text: String,
}

/// The lines of this run, in the order a console showed them. A window reads
/// them: the report of a GUI run is the window, so the whole run, including
/// `GitHub token taken from the secure store.`, is readable there and never in a
/// box. Progress replaces the progress before it, which is what \r does in a
/// terminal, and the cap keeps a long run's memory flat.
static LINES: Mutex<Vec<Line>> = Mutex::new(Vec::new());

/// Bumped with every line, so a window notices new text without copying the list
/// on every frame.
static REVISION: AtomicU64 = AtomicU64::new(0);

/// True while a progress line is the one being written to: the console rule
/// closes it with a newline, and the window rule starts a fresh line after it.
static PROGRESS_OPEN: AtomicBool = AtomicBool::new(false);

const CAP: usize = 2000;

fn record(level: Level, message: &str) {
    // Quiet means "no chatter" to a window as well as to a console; problems are
    // never chatter.
    if is_quiet() && !matches!(level, Level::Warn | Level::Error) {
        return;
    }
    let mut lines = LINES.lock().unwrap_or_else(|guard| guard.into_inner());
    let continues = level == Level::Progress
        && PROGRESS_OPEN.load(Ordering::Relaxed)
        && matches!(lines.last(), Some(last) if last.level == Level::Progress);
    if continues {
        if let Some(last) = lines.last_mut() {
            last.text = message.to_string();
        }
    } else {
        lines.push(Line { level, text: message.to_string() });
        let past = lines.len().saturating_sub(CAP);
        if past > 0 {
            lines.drain(0..past);
        }
    }
    if level == Level::Progress {
        PROGRESS_OPEN.store(true, Ordering::Relaxed);
    }
    REVISION.fetch_add(1, Ordering::Relaxed);
}

/// Everything the console would be showing, oldest first.
pub fn lines() -> Vec<Line> {
    LINES
        .lock()
        .unwrap_or_else(|guard| guard.into_inner())
        .iter()
        .cloned()
        .collect()
}

/// The count of lines ever recorded. Read it, and only ask for `lines()` when it
/// moved.
pub fn revision() -> u64 {
    REVISION.load(Ordering::Relaxed)
}

pub fn info(message: &str) {
    record(Level::Info, message);
    if on_console() {
        println!("{message}");
    }
}

pub fn detail(message: &str) {
    record(Level::Detail, message);
    if on_console() {
        println!("    {message}");
    }
}

pub fn progress(message: &str) {
    record(Level::Progress, message);
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
    // The progress line is closed: what comes next is a line of its own, on the
    // console by the newline, in the window by the rule in `record`.
    PROGRESS_OPEN.store(false, Ordering::Relaxed);
    if on_console() {
        println!();
    }
    LAST_WIDTH.store(0, Ordering::Relaxed);
}

pub fn warn(message: &str) {
    record(Level::Warn, message);
    if is_console_output() {
        eprintln!("warning: {message}");
    }
}

pub fn error(message: &str) {
    record(Level::Error, message);
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

    #[test]
    fn a_window_reads_the_run_and_the_capture_stays_bounded() {
        // The shape of one source: chatter, a progress line moved through, and a
        // warning. The prefix keeps these lines findable in a list nothing else
        // writes to - no test calls log, so this is the only writer.
        detail("capture-test: GitHub token taken from the secure store.");
        progress("capture-test: probe.zip  50%");
        progress("capture-test: probe.zip  100%");
        progress_done();
        warn("capture-test: a warning");

        let mine: Vec<String> = lines()
            .iter()
            .filter(|line| line.text.starts_with("capture-test:"))
            .map(|line| line.text.clone())
            .collect();
        assert_eq!(
            mine,
            vec![
                // The token's provenance is in the list, which is the whole
                // point: a GUI run can read it in the window.
                "capture-test: GitHub token taken from the secure store.".to_string(),
                // The progress line was moved through, not stacked up, and the
                // progress_done() closed it.
                "capture-test: probe.zip  100%".to_string(),
                // A warning is a line of its own.
                "capture-test: a warning".to_string(),
            ]
        );
        assert!(revision() > 0);

        // Past the cap the oldest lines go, the newest stay.
        for index in 0..CAP + 500 {
            info(&format!("capture-cap: {index}"));
        }
        let kept_lines = lines();
        let kept: Vec<&str> = kept_lines
            .iter()
            .filter_map(|line| line.text.strip_prefix("capture-cap: "))
            .collect();
        assert!(kept.len() <= CAP, "{} lines kept, the cap is {}", kept.len(), CAP);
        let newest = format!("{}", CAP + 499);
        assert_eq!(kept.last().copied(), Some(newest.as_str()));
        assert!(!kept.contains(&"0"));
    }
}
