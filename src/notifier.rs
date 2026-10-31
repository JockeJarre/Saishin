use crate::log;
use std::process::Command;

/// `CREATE_NO_WINDOW`: run a child without giving it a console. A GUI run has
/// none, so without this each notification and sound gets a window of its own.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

/// The child processes above, with the console flag applied where it exists.
#[cfg(windows)]
fn no_console(mut command: Command) -> Command {
    use std::os::windows::process::CommandExt;
    command.creation_flags(CREATE_NO_WINDOW);
    command
}

#[cfg(not(windows))]
fn no_console(command: Command) -> Command {
    command
}

pub fn send(title: &str, body: &str) {
    let result = try_send(title, body);
    if let Err(ex) = result {
        log::warn(&format!("Notification failed: {ex}"));
    }
}

fn try_send(title: &str, body: &str) -> Result<(), String> {
    if cfg!(windows) {
        let script = format!(
            "[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] | Out-Null; \
             $x = [Windows.UI.Notifications.ToastNotificationManager]::GetTemplateContent([Windows.UI.Notifications.ToastTemplateType]::ToastText02); \
             $t = $x.GetElementsByTagName('text'); \
             $t.Item(0).AppendChild($x.CreateTextNode({})) | Out-Null; \
             $t.Item(1).AppendChild($x.CreateTextNode({})) | Out-Null; \
             [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('saishin').Show($x)",
            ps_quote(title),
            ps_quote(body)
        );
        run("powershell", &["-NoProfile", "-NonInteractive", "-Command", &quote(&script)])
    } else if cfg!(target_os = "macos") {
        run(
            "osascript",
            &[
                "-e",
                &format!(
                    "display notification {} with title {}",
                    quote(body),
                    quote(title)
                ),
            ],
        )
    } else {
        run("notify-send", &[&quote(title), &quote(body)])
    }
}

pub fn play_sound(path: &str, player: &str) {
    let result = try_play_sound(path, player);
    if let Err(ex) = result {
        log::warn(&format!("Sound failed: {ex}"));
    }
}

fn try_play_sound(path: &str, player: &str) -> Result<(), String> {
    if !player.is_empty() {
        let mut parts = player.splitn(2, ' ');
        let file_name = parts.next().unwrap_or(player);
        let rest = parts.next().unwrap_or("");
        let mut command = Command::new(file_name);
        if !rest.is_empty() {
            command.args(rest.split_whitespace());
        }
        command.arg(path);
        match no_console(command).output() {
            Ok(_) => Ok(()),
            Err(ex) => Err(first_line(&ex.to_string())),
        }
    } else if cfg!(target_os = "macos") {
        run("afplay", &[path])
    } else {
        run("ffplay", &["-nodisp", "-autoexit", "-loglevel", "quiet", path])
    }
}

fn run(file_name: &str, args: &[&str]) -> Result<(), String> {
    let mut command = Command::new(file_name);
    command.args(args);
    match no_console(command).output() {
        Ok(_) => Ok(()),
        Err(ex) => Err(first_line(&ex.to_string())),
    }
}

fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\\\""))
}

fn ps_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn first_line(message: &str) -> String {
    message
        .split(['\n', '\r'])
        .next()
        .unwrap_or(message)
        .trim()
        .to_string()
}

/// Words no console can carry: a box on Windows, a notification elsewhere.
pub fn alert(title: &str, body: &str) {
    #[cfg(windows)]
    message_box(title, body, MB_ICONERROR);
    #[cfg(not(windows))]
    send(title, body);
}

/// A box with a question and two answers: the run has something to ask, not only
/// to say, and cannot go on before it is answered. True is Yes. Where no box can
/// carry a question, the words go into a notification and the answer is No: a
/// notification cannot be answered, and a run must not hang on one.
pub fn ask(title: &str, body: &str) -> bool {
    #[cfg(windows)]
    {
        message_box(title, body, MB_ICONQUESTION | MB_YESNO) == IDYES
    }
    #[cfg(not(windows))]
    {
        send(title, body);
        false
    }
}

#[cfg(windows)]
const MB_ICONERROR: u32 = 0x0000_0010;
#[cfg(windows)]
const MB_ICONQUESTION: u32 = 0x0000_4000;
#[cfg(windows)]
const MB_YESNO: u32 = 0x0000_0004;
/// `MessageBoxW`'s answer for the Yes button.
#[cfg(windows)]
const IDYES: i32 = 6;

/// One modal box drawn by the system: no child process, and this process stays
/// alive while the box is up, so the text can be read and a button pressed. What
/// comes back is the button, 0 for a box with no buttons to press.
#[cfg(windows)]
fn message_box(title: &str, body: &str, kind: u32) -> i32 {
    // User32 is already linked by the windowing stack.
    #[link(name = "user32")]
    extern "system" {
        fn MessageBoxW(
            hwnd: *const core::ffi::c_void,
            text: *const u16,
            caption: *const u16,
            kind: u32,
        ) -> i32;
    }

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    let caption = wide(title);
    let text = wide(body);
    // Safety: both strings are NUL-terminated and outlive the call.
    unsafe { MessageBoxW(core::ptr::null(), text.as_ptr(), caption.as_ptr(), kind) }
}
