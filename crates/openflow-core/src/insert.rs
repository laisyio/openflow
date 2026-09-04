//! Getting text from OpenFlow into the app the user is actually working in.
//!
//! Two mechanisms (clipboard-and-paste, or synthesized keystrokes) crossed with
//! two policies for what happens to whatever the user had copied before.

use std::time::Duration;

/// How text reaches the app the user is working in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InsertMethod {
    /// Put it on the clipboard, then send Cmd+V. Universal, because the
    /// keystroke is real and the app does the rest, but it overwrites whatever
    /// the user had copied.
    Paste,
    /// Synthesize keystrokes carrying the text itself. Leaves the clipboard
    /// alone and skips the paste round-trip, but the receiving app's
    /// autocorrect gets a say -- Notes rewrites `english` as `English`.
    Type,
}

impl InsertMethod {
    pub fn from_setting(value: Option<String>) -> Self {
        match value.as_deref().map(str::trim) {
            Some("type") => Self::Type,
            _ => Self::Paste,
        }
    }
}

/// What happens to whatever the user had copied before an insertion.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ClipboardPolicy {
    /// Put the previous contents back once the insertion has consumed them.
    Restore,
    /// Leave the transcript on the clipboard: the user asked for it there.
    Keep,
}

impl ClipboardPolicy {
    /// The `preserve_clipboard` setting. On unless the user turned it off.
    pub fn from_setting(value: Option<String>) -> Self {
        match value.as_deref().map(str::trim) {
            Some("false") => Self::Keep,
            _ => Self::Restore,
        }
    }
}

pub fn write_clipboard(text: &str) -> Result<(), String> {
    use arboard::Clipboard;
    let mut clipboard = Clipboard::new().map_err(|e| format!("Clipboard init failed: {}", e))?;
    clipboard
        .set_text(text)
        .map_err(|e| format!("Clipboard set failed: {}", e))
}

/// Put `text` where the user is typing. Correct for the tray and the re-copy
/// hotkey, where focus is in the user's editor; wrong for a list inside
/// OpenFlow, which would insert into OpenFlow itself.
///
/// Under `Keep` the clipboard is written either way; for `Type` it is then a
/// safety net rather than the delivery mechanism. Under `Restore`, `Type`
/// never touches the clipboard at all, and `Paste` puts the previous contents
/// back once the target app has had time to read the transcript. A paste that
/// macOS blocks leaves the transcript on the clipboard, since the error text
/// tells the user it is there.
pub fn paste_to_clipboard(
    text: &str,
    method: InsertMethod,
    policy: ClipboardPolicy,
) -> Result<(), String> {
    match (method, policy) {
        (InsertMethod::Type, ClipboardPolicy::Restore) => type_text(text),
        (InsertMethod::Type, ClipboardPolicy::Keep) => {
            write_clipboard(text)?;
            type_text(text)
        }
        (InsertMethod::Paste, ClipboardPolicy::Keep) => {
            write_clipboard(text)?;
            simulate_paste()
        }
        (InsertMethod::Paste, ClipboardPolicy::Restore) => {
            let previous = snapshot_clipboard();
            write_clipboard(text)?;
            simulate_paste()?;
            schedule_clipboard_restore(previous, text.to_string());
            Ok(())
        }
    }
}

/// What the clipboard held before we replaced it. Text and images round-trip
/// through the clipboard crate; files and rich content do not, so they are
/// reported as `Other` and cannot be put back.
pub enum ClipboardSnapshot {
    Text(String),
    Image(arboard::ImageData<'static>),
    Other,
}

pub fn snapshot_clipboard() -> ClipboardSnapshot {
    let Ok(mut clipboard) = arboard::Clipboard::new() else {
        return ClipboardSnapshot::Other;
    };
    if let Ok(text) = clipboard.get_text() {
        return ClipboardSnapshot::Text(text);
    }
    if let Ok(image) = clipboard.get_image() {
        return ClipboardSnapshot::Image(image.to_owned_img());
    }
    ClipboardSnapshot::Other
}

/// How long the target app gets to read the clipboard after Cmd+V lands.
/// The keystroke is delivered before `simulate_paste` returns, but the app
/// reads the pasteboard when it handles the event, and heavy apps take a
/// few hundred milliseconds to get there.
pub const CLIPBOARD_RESTORE_DELAY: Duration = Duration::from_millis(500);

/// Put `previous` back after the grace period, unless the clipboard no longer
/// holds our transcript, which means the user copied something newer.
pub fn schedule_clipboard_restore(previous: ClipboardSnapshot, ours: String) {
    if matches!(previous, ClipboardSnapshot::Other) {
        return;
    }
    std::thread::spawn(move || {
        std::thread::sleep(CLIPBOARD_RESTORE_DELAY);
        let Ok(mut clipboard) = arboard::Clipboard::new() else {
            return;
        };
        let current = clipboard.get_text().ok();
        if !clipboard_still_ours(current.as_deref(), &ours) {
            return;
        }
        let _ = match previous {
            ClipboardSnapshot::Text(text) => clipboard.set_text(text),
            ClipboardSnapshot::Image(image) => clipboard.set_image(image),
            ClipboardSnapshot::Other => Ok(()),
        };
    });
}

/// The restore must never clobber something the user copied in the meantime.
fn clipboard_still_ours(current: Option<&str>, ours: &str) -> bool {
    current == Some(ours)
}

/// What a `type_text` call will post, or `None` when it will post nothing.
///
/// The empty case is decided here rather than inline because `type_text` itself
/// cannot be asked about it: posting the stray `a` and posting nothing both
/// return `Ok(())`, and the difference only shows up as a character in whatever
/// window happened to be focused.
#[cfg(target_os = "macos")]
fn keystroke_payload(text: &str) -> Option<&str> {
    (!text.is_empty()).then_some(text)
}

/// Send `text` as a synthesized keystroke, unicode payload and all.
///
/// Three things here are load-bearing, each one measured rather than assumed:
///
/// - **Empty text returns early.** `set_string("")` does not neutralize the
///   event, it leaves virtual keycode 0 to mean what it normally means, and
///   types a stray `a`.
/// - **One event carries the whole string.** 504 characters arrived intact in
///   ~60us, so there is nothing to gain by chunking and a dropped chunk to lose.
/// - **The warm-up is not decoration.** The first post of a process costs ~40ms,
///   and without paying it first the opening segment is swallowed -- silently,
///   and always at the start of the text.
#[cfg(target_os = "macos")]
pub fn type_text(text: &str) -> Result<(), String> {
    use core_graphics::event::{CGEvent, CGEventTapLocation};
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};

    let Some(text) = keystroke_payload(text) else {
        return Ok(());
    };

    let source = || {
        CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|_| "Could not reach the window server to type the text.".to_string())
    };

    // Warm up, then let the focused window settle before the real event.
    let _ = source()?;
    std::thread::sleep(Duration::from_millis(20));

    let src = source()?;
    let down = CGEvent::new_keyboard_event(src.clone(), 0, true)
        .map_err(|_| "Could not create the keystroke.".to_string())?;
    down.set_string(text);
    down.post(CGEventTapLocation::HID);

    let up = CGEvent::new_keyboard_event(src, 0, false)
        .map_err(|_| "Could not create the keystroke.".to_string())?;
    up.set_string(text);
    up.post(CGEventTapLocation::HID);
    Ok(())
}

/// Pay the first CGEvent post's cost now, while the user is still talking.
///
/// A process pays ~40ms for its first post -- 32-44ms measured here -- and
/// `type_text` was paying it immediately before the real keystroke, which is
/// the worst possible moment: the transcript is already back and the user is
/// waiting on the text. Posting one inert event when the capture starts moves
/// that cost into the seconds spent speaking, and the next post takes ~16us.
///
/// Keycode 255 is not a key any keyboard reports and no unicode payload is set,
/// so nothing reaches the focused app. Two alternatives were measured and
/// rejected: a modifier leaves 12-16ms on the next post, and `Fn` can be bound
/// to the emoji picker or an input-source switch.
///
/// Additive, never load-bearing. The warm-up inside `type_text` stays exactly
/// where it is, so a capture that never prewarmed -- a different insert method,
/// a failure here -- still pays the cost itself and nothing regresses.
#[cfg(target_os = "macos")]
pub fn prewarm_typing() {
    use core_graphics::event::{CGEvent, CGEventTapLocation};
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};

    std::thread::spawn(|| {
        let Ok(source) = CGEventSource::new(CGEventSourceStateID::HIDSystemState) else {
            return;
        };
        let Ok(event) = CGEvent::new_keyboard_event(source, 255, true) else {
            return;
        };
        event.post(CGEventTapLocation::HID);
    });
}

#[cfg(not(target_os = "macos"))]
pub fn prewarm_typing() {}

/// Only macOS has a verified implementation. Everywhere else `Type` behaves as
/// `Paste` rather than shipping a guess about how synthesized input behaves on
/// a platform nobody tested.
#[cfg(not(target_os = "macos"))]
pub fn type_text(_text: &str) -> Result<(), String> {
    simulate_paste()
}

#[cfg(target_os = "macos")]
pub fn simulate_paste() -> Result<(), String> {
    use std::process::Command;
    std::thread::sleep(std::time::Duration::from_millis(200));
    let status = Command::new("osascript")
        .arg("-e")
        .arg("tell application \"System Events\" to keystroke \"v\" using command down")
        .status()
        .map_err(|e| format!("Could not paste at the cursor: {}", e))?;
    if status.success() {
        Ok(())
    } else {
        Err("Text was copied, but macOS blocked automatic paste. Grant OpenFlow Accessibility access.".to_string())
    }
}

#[cfg(target_os = "windows")]
pub fn simulate_paste() -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    use std::process::Command;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let status = Command::new("powershell")
        .arg("-Command")
        .arg("Add-Type -AssemblyName System.Windows.Forms; [System.Windows.Forms.SendKeys]::SendWait('^v')")
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map_err(|e| format!("Could not paste at the cursor: {}", e))?;
    if status.success() {
        Ok(())
    } else {
        Err("Text was copied, but Windows blocked automatic paste.".to_string())
    }
}

#[cfg(target_os = "linux")]
pub fn simulate_paste() -> Result<(), String> {
    use std::process::Command;
    let result = Command::new("xdotool")
        .arg("key")
        .arg("ctrl+v")
        .output()
        .or_else(|_| {
            Command::new("ydotool")
                .arg("key")
                .arg("29:1")
                .arg("47:1")
                .arg("47:0")
                .arg("29:0")
                .output()
        })
        .map_err(|e| format!("Could not paste at the cursor: {}", e))?;
    if result.status.success() {
        Ok(())
    } else {
        Err("Text was copied, but the desktop blocked automatic paste.".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_is_preserved_unless_the_user_opted_out() {
        assert_eq!(
            ClipboardPolicy::from_setting(None),
            ClipboardPolicy::Restore
        );
        assert_eq!(
            ClipboardPolicy::from_setting(Some("true".to_string())),
            ClipboardPolicy::Restore
        );
        assert_eq!(
            ClipboardPolicy::from_setting(Some(" false ".to_string())),
            ClipboardPolicy::Keep
        );
    }

    #[test]
    fn restore_yields_to_anything_the_user_copied_meanwhile() {
        assert!(clipboard_still_ours(
            Some("the transcript"),
            "the transcript"
        ));
        assert!(!clipboard_still_ours(
            Some("a newer copy"),
            "the transcript"
        ));
        assert!(!clipboard_still_ours(None, "the transcript"));
    }

    #[test]
    fn insert_method_defaults_to_paste() {
        // Anything that is not exactly "type" is Paste, so an unset setting, a
        // stale value, or a typo degrades to the universal path rather than to
        // the one whose behaviour depends on the receiving app.
        assert_eq!(InsertMethod::from_setting(None), InsertMethod::Paste);
        assert_eq!(
            InsertMethod::from_setting(Some(String::new())),
            InsertMethod::Paste
        );
        assert_eq!(
            InsertMethod::from_setting(Some("paste".to_string())),
            InsertMethod::Paste
        );
        assert_eq!(
            InsertMethod::from_setting(Some("typing".to_string())),
            InsertMethod::Paste
        );
        assert_eq!(
            InsertMethod::from_setting(Some("type".to_string())),
            InsertMethod::Type
        );
        assert_eq!(
            InsertMethod::from_setting(Some("  type  ".to_string())),
            InsertMethod::Type
        );
    }

    /// Empty text must not reach CGEvent. `set_string("")` leaves virtual
    /// keycode 0 holding its normal meaning, so the "empty" keystroke types
    /// a literal `a`. Observed, not theorised.
    #[test]
    #[cfg(target_os = "macos")]
    fn typing_empty_text_posts_nothing() {
        assert_eq!(keystroke_payload(""), None);
        assert_eq!(keystroke_payload("the transcript"), Some("the transcript"));
    }
}
