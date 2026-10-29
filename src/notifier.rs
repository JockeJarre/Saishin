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
