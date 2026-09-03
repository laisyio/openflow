//! "Open at login": registering the app itself with `SMAppService`.
//!
//! The only place in the crate that talks to ServiceManagement. macOS owns this
//! state, so nothing about it is stored in `Settings`: the switch on the
//! Settings page is a view of [`state`], and every write goes back to
//! [`set_enabled`] and then re-reads. A stored copy could disagree with the
//! system the moment the user turned the item off in System Settings, and there
//! would then be no way to tell which of the two was right.
//!
//! macOS 13 is the floor for `SMAppService`, which is also the app's floor.
//! Below it every call here would be a missing symbol, so there is no fallback
//! to the deprecated `SMLoginItemSetEnabled` or to a LaunchAgent plist.
//!
//! Registration can fail, and the failure is a message rather than a panic. It
//! is not reached by the obvious candidate, though: measured on macOS 26 with
//! an ad hoc signature, a `cargo run` of the bare `target/debug` binary
//! registers happily, and the item macOS then remembers points at that path
//! rather than at an app bundle. So the failure branch is there for what macOS
//! actually refuses, and the switch reverts to whatever [`state`] reports next
//! rather than to whatever the user just clicked.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

/// What macOS says about this app's login item, one variant per
/// `SMAppServiceStatus`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Not registered. The normal "off".
    Off,
    /// Registered and allowed to launch.
    On,
    /// Registered, but the user has to approve it in System Settings before it
    /// will actually launch. Reached when login items were disabled for this
    /// app before, since macOS remembers that refusal.
    RequiresApproval,
    /// macOS cannot find a service for this bundle. What an unbundled binary
    /// gets, and what a registration that never landed leaves behind.
    NotFound,
}

/// The note text under the switch when the state needs explaining.
const APPROVAL_NOTE: &str = "Approve OpenFlow under System Settings > General > Login Items.";

/// How a [`State`] shows up on the Settings page: whether the switch is on, and
/// the line to put under it.
///
/// Pure, and separate from the two calls above it so the mapping can be tested
/// on any platform. `RequiresApproval` reads as on because the app is
/// registered and the user did ask for it; the note says what is still missing.
/// `NotFound` reads as off because nothing is registered.
pub fn presentation(state: State) -> (bool, &'static str) {
    match state {
        State::Off => (false, ""),
        State::On => (true, ""),
        State::RequiresApproval => (true, APPROVAL_NOTE),
        State::NotFound => (false, ""),
    }
}

#[cfg(target_os = "macos")]
mod sys {
    use objc2_service_management::{SMAppService, SMAppServiceStatus};

    use super::State;

    /// Read the current login item state from macOS.
    ///
    /// Any status the SDK grows later reads as [`State::Off`], and says so
    /// under `OPENFLOW_TRACE=1`.
    pub fn state() -> State {
        // SAFETY: `mainAppService` takes no arguments and returns a retained
        // `SMAppService` for this bundle; `status` reads a scalar off it. The
        // class is not main-thread-only, so no marker is needed.
        let status = unsafe { SMAppService::mainAppService().status() };
        match status {
            SMAppServiceStatus::NotRegistered => State::Off,
            SMAppServiceStatus::Enabled => State::On,
            SMAppServiceStatus::RequiresApproval => State::RequiresApproval,
            SMAppServiceStatus::NotFound => State::NotFound,
            other => {
                // An unknown number is not a reason to claim the app launches
                // at login, but it is a reason to be able to find out: without
                // the raw value there is nothing to look up in the SDK.
                crate::trace!("login item: unknown SMAppServiceStatus {}", other.0);
                State::Off
            }
        }
    }

    /// Register or unregister the app as a login item, then report where that
    /// left it.
    ///
    /// The state comes back from a fresh [`state`] read rather than from
    /// whether the call returned `Ok`: registering can succeed and still land
    /// on `RequiresApproval`, and only macOS knows which.
    ///
    /// On failure the error's `localizedDescription` comes back as the message
    /// for the Settings page to show. Nothing here panics.
    pub fn set_enabled(on: bool) -> Result<State, String> {
        // SAFETY: as above, plus both methods take only the implicit error
        // out-parameter that `objc2` turns into the `Result`.
        let result = unsafe {
            let service = SMAppService::mainAppService();
            if on {
                service.registerAndReturnError()
            } else {
                service.unregisterAndReturnError()
            }
        };
        match result {
            Ok(()) => Ok(state()),
            Err(error) => Err(error.localizedDescription().to_string()),
        }
    }
}

#[cfg(target_os = "macos")]
pub use sys::{set_enabled, state};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_and_not_found_read_as_off_with_no_note() {
        assert_eq!(presentation(State::Off), (false, ""));
        assert_eq!(presentation(State::NotFound), (false, ""));
    }

    #[test]
    fn enabled_reads_as_on_with_no_note() {
        assert_eq!(presentation(State::On), (true, ""));
    }

    #[test]
    fn requires_approval_reads_as_on_and_says_where_to_approve() {
        let (on, note) = presentation(State::RequiresApproval);
        assert!(on, "the app is registered, so the switch stays on");
        assert!(
            note.contains("Login Items"),
            "the note has to name the pane the user has to visit, got {note:?}"
        );
    }

    /// The note belongs to exactly one state. Without this, changing
    /// `APPROVAL_NOTE` to the empty string would leave every assertion above
    /// still passing.
    #[test]
    fn only_requires_approval_carries_a_note() {
        let noted: Vec<State> = [
            State::Off,
            State::On,
            State::RequiresApproval,
            State::NotFound,
        ]
        .into_iter()
        .filter(|state| !presentation(*state).1.is_empty())
        .collect();
        assert_eq!(noted, vec![State::RequiresApproval]);
    }
}
