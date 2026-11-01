//! The words a run has to put in front of a person when no console is left to
//! carry them. One box, drawn by the system: the module starts no program, so
//! there is no child window, no shell line built out of a run's own text, and
//! nothing that can answer "program not found".

/// Words no console can carry, in a box with an error icon.
#[cfg(windows)]
pub fn alert(title: &str, body: &str) {
    message_box(title, body, MB_ICONERROR);
}

/// A box with a question and two answers: the run has something to ask, not only
/// to say, and cannot go on before it is answered. True is Yes. The box is modal,
/// so the run waits, and the window it is waiting for is the one it is standing
/// in front of.
#[cfg(windows)]
pub fn ask(title: &str, body: &str) -> bool {
    message_box(title, body, MB_ICONQUESTION | MB_YESNO) == IDYES
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
