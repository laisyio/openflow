//! Bounded Accessibility snapshots and closed-set actions. No screen pixels,
//! field values, shell commands, coordinates, or synthetic Return key.
use openflow_core::computer_use::{Element, Proposal, Screen, MAX_ELEMENTS};
use std::collections::VecDeque;
use std::ffi::{c_char, c_void, CString};
use std::time::{Duration, Instant};

type Ref = *const c_void;
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXUIElementCreateApplication(pid: i32) -> Ref;
    fn AXUIElementGetTypeID() -> usize;
    fn AXUIElementCopyAttributeValue(element: Ref, attribute: Ref, value: *mut Ref) -> i32;
    fn AXUIElementCopyActionNames(element: Ref, names: *mut Ref) -> i32;
    fn AXUIElementIsAttributeSettable(element: Ref, attribute: Ref, settable: *mut u8) -> i32;
    fn AXUIElementSetAttributeValue(element: Ref, attribute: Ref, value: Ref) -> i32;
    fn AXUIElementPerformAction(element: Ref, action: Ref) -> i32;
    fn AXUIElementSetMessagingTimeout(element: Ref, timeout: f32) -> i32;
    fn AXValueGetTypeID() -> usize;
    fn AXValueGetType(value: Ref) -> u32;
    fn AXValueGetValue(value: Ref, kind: u32, result: *mut c_void) -> u8;
}
#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(value: Ref);
    fn CFRetain(value: Ref) -> Ref;
    fn CFEqual(a: Ref, b: Ref) -> u8;
    fn CFGetTypeID(value: Ref) -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFArrayGetTypeID() -> usize;
    fn CFStringCreateWithCString(allocator: Ref, text: *const c_char, encoding: u32) -> Ref;
    fn CFStringGetLength(value: Ref) -> isize;
    fn CFStringGetCString(value: Ref, buffer: *mut c_char, size: isize, encoding: u32) -> u8;
    fn CFArrayGetCount(value: Ref) -> isize;
    fn CFArrayGetValueAtIndex(value: Ref, index: isize) -> Ref;
    static kCFBooleanTrue: Ref;
    static kCFBooleanFalse: Ref;
}

// Owned CF create/copy reference, intentionally neither Send nor Sync. All AX
// handles live on the AppKit main thread and never cross the network task.
struct Owned(Ref);
impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0) }
        }
    }
}
impl Owned {
    fn selection(&self) -> Option<(isize, isize)> {
        #[repr(C)]
        struct Range {
            location: isize,
            length: isize,
        }
        let value = self.attribute("AXSelectedTextRange")?;
        if unsafe { CFGetTypeID(value.0) != AXValueGetTypeID() || AXValueGetType(value.0) != 4 } {
            return None;
        }
        let mut range = Range {
            location: 0,
            length: 0,
        };
        if unsafe { AXValueGetValue(value.0, 4, (&mut range as *mut Range).cast()) } == 0
            || range.location < 0
            || range.length < 0
        {
            return None;
        }
        Some((range.location, range.length))
    }
    fn same(&self, other: &Self) -> bool {
        unsafe { CFEqual(self.0, other.0) != 0 }
    }
    fn text(text: &str) -> Self {
        let c = CString::new(text).expect("validated string has no NUL");
        Self(unsafe { CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), 0x08000100) })
    }
    fn attribute(&self, name: &str) -> Option<Self> {
        let name = Self::text(name);
        let mut value = std::ptr::null();
        (unsafe { AXUIElementCopyAttributeValue(self.0, name.0, &mut value) } == 0
            && !value.is_null())
        .then_some(value)
        .map(Self)
    }
    fn string(&self) -> Option<String> {
        if unsafe { CFGetTypeID(self.0) != CFStringGetTypeID() } {
            return None;
        }
        let length = unsafe { CFStringGetLength(self.0) };
        if !(0..=512).contains(&length) {
            return None;
        }
        let mut bytes = vec![0u8; length as usize * 4 + 1];
        if unsafe {
            CFStringGetCString(
                self.0,
                bytes.as_mut_ptr().cast(),
                bytes.len() as isize,
                0x08000100,
            )
        } == 0
        {
            return None;
        }
        let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
        String::from_utf8(bytes[..end].to_vec()).ok()
    }
    fn name(&self, attribute: &str) -> String {
        self.attribute(attribute)
            .and_then(|a| a.string())
            .unwrap_or_default()
    }
    fn array(&self, limit: usize) -> Vec<Self> {
        if unsafe { CFGetTypeID(self.0) != CFArrayGetTypeID() } {
            return Vec::new();
        }
        (0..unsafe { CFArrayGetCount(self.0) }.min(limit as isize))
            .filter_map(|i| {
                let p = unsafe { CFArrayGetValueAtIndex(self.0, i) };
                (!p.is_null()).then(|| Self(unsafe { CFRetain(p) }))
            })
            .collect()
    }
    fn settable(&self, name: &str) -> bool {
        let name = Self::text(name);
        let mut yes = 0;
        unsafe { AXUIElementIsAttributeSettable(self.0, name.0, &mut yes) == 0 && yes != 0 }
    }
    fn actions(&self) -> Vec<String> {
        let mut names = std::ptr::null();
        if unsafe { AXUIElementCopyActionNames(self.0, &mut names) } != 0 || names.is_null() {
            return Vec::new();
        }
        Self(names)
            .array(32)
            .iter()
            .filter_map(Self::string)
            .collect()
    }
}

pub struct Snapshot {
    pub screen: Screen,
    pub pid: i32,
    pub window_label: String,
    window: Owned,
    handles: Vec<Owned>,
    selections: Vec<Option<(isize, isize)>>,
    created: Instant,
}

pub fn trusted() -> bool {
    unsafe { AXIsProcessTrusted() }
}

pub fn snapshot(pid: i32, application: &str) -> Result<Snapshot, String> {
    if !trusted() {
        return Err("Enable OpenFlow in System Settings → Privacy & Security → Accessibility, then try again.".into());
    }
    if pid <= 0 || pid == std::process::id() as i32 {
        return Err(
            "Switch to the app you want to control, then open Computer use from the menu bar."
                .into(),
        );
    }
    let root = Owned(unsafe { AXUIElementCreateApplication(pid) });
    if root.0.is_null() {
        return Err("The target app is unavailable.".into());
    }
    unsafe {
        AXUIElementSetMessagingTimeout(root.0, 0.05);
    }
    let window = root
        .attribute("AXFocusedWindow")
        .ok_or("The target app has no accessible focused window.")?;
    unsafe {
        AXUIElementSetMessagingTimeout(window.0, 0.05);
    }
    let window_label = window.name("AXTitle");
    let mut queue = VecDeque::from([(Owned(unsafe { CFRetain(window.0) }), 0)]);
    let mut elements = Vec::new();
    let mut handles = Vec::new();
    let mut selections = Vec::new();
    let started = Instant::now();
    let mut visited = 0;
    while let Some((item, depth)) = queue.pop_front() {
        visited += 1;
        if visited > 512
            || elements.len() >= MAX_ELEMENTS
            || started.elapsed() > Duration::from_millis(800)
        {
            break;
        }
        if unsafe { CFGetTypeID(item.0) != AXUIElementGetTypeID() } {
            continue;
        }
        // The timeout belongs to each AX object, not just the application root.
        unsafe {
            AXUIElementSetMessagingTimeout(item.0, 0.05);
        }
        let role = item.name("AXRole");
        let subrole = item.name("AXSubrole");
        if role.contains("Secure") || subrole.contains("Secure") {
            continue;
        }
        let enabled = item
            .attribute("AXEnabled")
            .is_none_or(|v| unsafe { CFEqual(v.0, kCFBooleanFalse) == 0 });
        let available = item.actions();
        let mut actions = Vec::new();
        let mut selection = None;
        if enabled {
            for (name, action) in [
                ("AXPress", "press"),
                ("AXScrollDownByPage", "scroll_down"),
                ("AXScrollUpByPage", "scroll_up"),
            ] {
                if available.iter().any(|a| a == name) {
                    actions.push(action.to_string());
                }
            }
            if ["AXTextField", "AXTextArea", "AXComboBox"].contains(&role.as_str()) {
                if item.settable("AXFocused") {
                    actions.push("focus".into());
                }
                if item.settable("AXSelectedText") {
                    selection = item.selection();
                    if selection.is_some() {
                        actions.push("type".into());
                    }
                }
            }
        }
        if depth < 12 {
            if let Some(children) = item.attribute("AXChildren") {
                for child in children.array(512usize.saturating_sub(visited + queue.len())) {
                    queue.push_back((child, depth + 1));
                }
            }
        }
        if actions.is_empty() {
            continue;
        }
        let label = ["AXTitle", "AXDescription", "AXPlaceholderValue"]
            .into_iter()
            .map(|key| item.name(key))
            .find(|s| !s.trim().is_empty())
            .unwrap_or_else(|| role.clone());
        elements.push(Element {
            id: format!("e{}", elements.len()),
            role,
            label: label
                .chars()
                .filter(|c| !c.is_control())
                .take(100)
                .collect(),
            actions,
        });
        handles.push(item);
        selections.push(selection);
    }
    if elements.is_empty() {
        return Err("This window exposes no supported controls. Try another app or window.".into());
    }
    Ok(Snapshot {
        screen: Screen {
            application: application.chars().take(100).collect(),
            elements,
        },
        pid,
        window,
        window_label,
        handles,
        selections,
        created: Instant::now(),
    })
}

impl Snapshot {
    /// A proposal is consumed by the caller before execution. Re-enumerate the
    /// original window; never resolve old numeric IDs against a different tree.
    pub fn execute(self, proposal: &Proposal) -> Result<(), String> {
        if self.created.elapsed() > Duration::from_secs(30) {
            return Err("The proposal expired. Review a new command.".into());
        }
        let fresh = snapshot(self.pid, &self.screen.application)?;
        let index = self.checked_target(&fresh, proposal)?;
        let target = &fresh.handles[index];
        let status = match proposal.action.as_str() {
            "focus" => unsafe {
                AXUIElementSetAttributeValue(target.0, Owned::text("AXFocused").0, kCFBooleanTrue)
            },
            "type" => {
                let text = proposal
                    .text
                    .as_deref()
                    .ok_or("No literal text to insert.")?;
                if text.is_empty()
                    || text.len() > openflow_core::computer_use::MAX_COMMAND
                    || text.chars().any(char::is_control)
                {
                    return Err("Invalid literal text.".into());
                }
                unsafe {
                    AXUIElementSetAttributeValue(
                        target.0,
                        Owned::text("AXSelectedText").0,
                        Owned::text(text).0,
                    )
                }
            }
            "press" => unsafe { AXUIElementPerformAction(target.0, Owned::text("AXPress").0) },
            "scroll_down" => unsafe {
                AXUIElementPerformAction(target.0, Owned::text("AXScrollDownByPage").0)
            },
            "scroll_up" => unsafe {
                AXUIElementPerformAction(target.0, Owned::text("AXScrollUpByPage").0)
            },
            _ => return Err("Unsupported action.".into()),
        };
        if status == 0 {
            Ok(())
        } else {
            Err(
                "The app refused that Accessibility action. Nothing will be retried automatically."
                    .into(),
            )
        }
    }

    fn checked_target(&self, fresh: &Self, proposal: &Proposal) -> Result<usize, String> {
        if self.created.elapsed() > Duration::from_secs(30) {
            return Err("The proposal expired. Review a new command.".into());
        }
        if !self.window.same(&fresh.window)
            || self.window_label != fresh.window_label
            || self.screen.elements != fresh.screen.elements
        {
            return Err("The target window changed. Review the command again.".into());
        }
        let index = self
            .screen
            .elements
            .iter()
            .position(|e| e.id == proposal.target)
            .ok_or("Unknown target.")?;
        let element = &self.screen.elements[index];
        if !element.actions.contains(&proposal.action)
            || !self.handles[index].same(&fresh.handles[index])
        {
            return Err("The target control changed.".into());
        }
        if proposal.action == "type"
            && (self.selections[index].is_none()
                || self.selections[index] != fresh.selections[index])
        {
            return Err("The text selection changed. Review the command again.".into());
        }
        Ok(index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // CF strings stand in for retained identities; no test queries another app
    // or invokes AX actions, requests permission, or accesses a microphone.
    fn fixture() -> Snapshot {
        Snapshot {
            screen: Screen {
                application: "Fixture".into(),
                elements: vec![Element {
                    id: "e0".into(),
                    role: "AXTextField".into(),
                    label: "Search".into(),
                    actions: vec!["focus".into(), "type".into()],
                }],
            },
            pid: 123,
            window_label: "Draft".into(),
            window: Owned::text("window-1"),
            handles: vec![Owned::text("control-1")],
            selections: vec![Some((3, 0))],
            created: Instant::now(),
        }
    }
    fn proposal() -> Proposal {
        Proposal {
            action: "type".into(),
            target: "e0".into(),
            text: Some("hello".into()),
        }
    }
    #[test]
    fn review_requires_same_window_document_tree_and_retained_target() {
        let original = fixture();
        assert_eq!(original.checked_target(&fixture(), &proposal()).unwrap(), 0);
        let mut fresh = fixture();
        fresh.window = Owned::text("window-2");
        assert!(original.checked_target(&fresh, &proposal()).is_err());
        let mut fresh = fixture();
        fresh.window_label = "Other draft".into();
        assert!(original.checked_target(&fresh, &proposal()).is_err());
        let mut fresh = fixture();
        fresh.screen.elements[0].label = "Other control".into();
        assert!(original.checked_target(&fresh, &proposal()).is_err());
        let mut fresh = fixture();
        fresh.handles[0] = Owned::text("replacement-control");
        assert!(original.checked_target(&fresh, &proposal()).is_err());
    }
    #[test]
    fn expired_changed_selection_and_unknown_capabilities_are_rejected() {
        let original = fixture();
        let mut fresh = fixture();
        fresh.selections[0] = Some((4, 2));
        assert!(original.checked_target(&fresh, &proposal()).is_err());
        let mut stale = fixture();
        stale.created = Instant::now() - Duration::from_secs(31);
        assert!(stale.checked_target(&fixture(), &proposal()).is_err());
        let mut p = proposal();
        p.action = "shell".into();
        assert!(original.checked_target(&fixture(), &p).is_err());
        p = proposal();
        p.target = "e9".into();
        assert!(original.checked_target(&fixture(), &p).is_err());
    }
    #[test]
    fn cf_strings_are_type_checked_and_bounded() {
        assert_eq!(
            Owned::text("hello 世界").string().as_deref(),
            Some("hello 世界")
        );
        assert_eq!(Owned::text(&"x".repeat(513)).string(), None);
        assert!(Owned::text("not an array").array(10).is_empty());
        drop(Owned(std::ptr::null()));
    }
}
