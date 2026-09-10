//! Global hotkeys, on the same `global-hotkey` crate the Tauri plugin wraps.
//!
//! Hold-to-talk is the whole reason this is not a "shortcut fired" API:
//! `Pressed` starts the capture and `Released` ends it, and the engine's
//! watchdog covers the case where a release is swallowed by another app.

use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, OnceLock};

use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};

use openflow_core::engine::Engine;
use openflow_core::hotkey;
use openflow_core::settings::Settings;

/// The two actions OpenFlow binds, in the order the settings window shows them.
pub const ACTIONS: [&str; 2] = ["record", "recopy"];

pub struct Hotkeys {
    manager: GlobalHotKeyManager,
    record: Option<HotKey>,
    recopy: Option<HotKey>,
    /// The binding a hotkey recorder has temporarily taken off the system, so
    /// pressing it types a chord instead of starting a capture. At most one,
    /// because only one recorder can be listening.
    suspended: Option<(String, HotKey)>,
}

impl Hotkeys {
    /// Register both bindings from settings. A binding that will not register
    /// (another app already owns the chord) is reported and skipped rather than
    /// taking the app down: the other one still works, and Settings can rebind.
    pub fn new(settings: &Settings) -> Result<Self, String> {
        let manager = GlobalHotKeyManager::new()
            .map_err(|error| format!("Could not reach the global hotkey service: {}", error))?;
        let mut hotkeys = Self {
            manager,
            record: None,
            recopy: None,
            suspended: None,
        };
        for action in ACTIONS {
            let shortcut = settings.shortcut(action)?;
            match hotkeys.manager.register(shortcut) {
                Ok(()) => hotkeys.remember(action, Some(shortcut)),
                Err(error) => eprintln!(
                    "The {} shortcut could not be registered: {}. Choose another one in Settings.",
                    action, error
                ),
            }
        }
        Ok(hotkeys)
    }

    fn remember(&mut self, action: &str, shortcut: Option<HotKey>) {
        match action {
            "record" => self.record = shortcut,
            "recopy" => self.recopy = shortcut,
            _ => {}
        }
    }

    fn current(&self, action: &str) -> Option<HotKey> {
        match action {
            "record" => self.record,
            "recopy" => self.recopy,
            _ => None,
        }
    }

    /// Release `action`'s chord while a recorder is listening for it.
    ///
    /// Without this, pressing the current record shortcut to re-record it would
    /// start a capture instead: the global registration wins, and the local
    /// event monitor never sees the key. The binding is remembered, not
    /// forgotten, and [`Self::resume`] puts it back.
    pub fn suspend(&mut self, action: &str) {
        self.resume();
        let Some(shortcut) = self.current(action) else {
            return;
        };
        if self.manager.unregister(shortcut).is_ok() {
            self.suspended = Some((action.to_string(), shortcut));
        }
    }

    /// Put back whatever [`Self::suspend`] took, if anything. A chord another
    /// app grabbed in the meantime leaves the action unbound rather than
    /// leaving the table claiming a registration that does not exist.
    pub fn resume(&mut self) {
        let Some((action, shortcut)) = self.suspended.take() else {
            return;
        };
        if self.manager.register(shortcut).is_err() {
            self.remember(&action, None);
        }
    }

    /// Point `action` at `shortcut_str` and save it. The new chord is
    /// registered before the old one is released, so a rejected chord leaves
    /// the action still working; and if saving fails the registration is rolled
    /// back, so the menu bar never disagrees with the database.
    pub fn rebind(
        &mut self,
        settings: &Settings,
        action: &str,
        shortcut_str: &str,
    ) -> Result<(), String> {
        if hotkey::default_shortcut(action).is_none() {
            return Err("Unknown hotkey action".to_string());
        }
        let new = hotkey::parse_shortcut(shortcut_str)
            .map_err(|error| format!("Invalid shortcut '{}': {}", shortcut_str, error))?;
        let old = self.current(action);
        if old == Some(new) {
            return settings.set_hotkey(action, shortcut_str);
        }
        self.manager
            .register(new)
            .map_err(|error| format!("Failed to register shortcut: {}", error))?;
        if let Some(old) = old {
            if let Err(error) = self.manager.unregister(old) {
                let _ = self.manager.unregister(new);
                return Err(format!("Could not replace the old shortcut: {}", error));
            }
        }
        if let Err(error) = settings.set_hotkey(action, shortcut_str) {
            let _ = self.manager.unregister(new);
            if let Some(old) = old {
                let _ = self.manager.register(old);
            }
            return Err(error);
        }
        self.remember(action, Some(new));
        Ok(())
    }

    /// Route one hotkey event. Runs on the main thread, after the hop.
    ///
    /// Only the part that has to be here stays here; every engine call goes to
    /// the worker. See [`route`] for why, and for what it costs.
    pub fn dispatch(&self, engine: &Arc<Engine>, event: GlobalHotKeyEvent) {
        if self.record.map(|key| key.id()) == Some(event.id)
            && matches!(event.state, HotKeyState::Pressed)
        {
            // "Paste the key, then hold the hotkey to try it" is the first
            // thing anyone does in Settings, and the overlay pill is a
            // non-activating panel, so nothing takes focus off the field.
            // Without this the capture would run against the key the user typed
            // but never committed, because credentials and endpoint URLs only
            // write when editing ends.
            //
            // This is the one part of a hotkey that is AppKit, so it is the one
            // part that stays on the main thread. It is synchronous and it runs
            // before the press is handed on, so the capture still sees the
            // committed value.
            crate::app::with_app(|app| app.with_settings(|window| window.commit_pending_edits()));
        }
        route(
            self.record.map(|key| key.id()),
            self.recopy.map(|key| key.id()),
            engine,
            event,
        );
    }
}

/// What a hotkey asks of the engine.
///
/// A trait rather than a direct call so the routing can be tested against a
/// stand-in: building a real [`Engine`] opens the database and the keychain,
/// and driving one would need a microphone.
pub trait HotkeyTarget: Clone + Send + 'static {
    fn pressed(&self);
    fn released(&self);
    fn recopy(&self);
}

impl HotkeyTarget for Arc<Engine> {
    fn pressed(&self) {
        Engine::hotkey_pressed(self);
    }

    fn released(&self) {
        Engine::hotkey_released(self);
    }

    fn recopy(&self) {
        Engine::recopy(self);
    }
}

/// Hand one hotkey event to `target`, off the calling thread.
///
/// **Why off it.** `dispatch` runs on the main thread -- `install_handler` hops
/// there through [`crate::events::on_main`] -- and all three engine calls block
/// it. The release is the one that costs the most: `Engine::hotkey_released`
/// waits on `AudioRecorder::stop`, a channel round trip to the audio thread,
/// which resamples, gains and WAV-encodes the whole take before it answers.
/// Measured on this machine in a release build that is 34 ms for a five second
/// take and 87 ms for a minute, and the 300 s ceiling `MAX_RECORDING` allows
/// costs about a quarter of a second. Nothing like the 5 s `recv_timeout` --
/// that is the give-up line, not the work -- but it is a quarter of a second in
/// which the app draws nothing, and it lands exactly when the user has stopped
/// talking and is watching the overlay for an answer. The press pays for
/// opening a `cpal` stream on the same thread, and the recopy for a 200 ms
/// settle plus an `osascript`.
///
/// **Why one worker and not a thread per event.** Press and release have to
/// reach the engine in the order the user made them. A thread each would let a
/// press that lands while the previous release is still encoding overtake it,
/// and the states the overlay shows would arrive out of order. One serial
/// worker keeps the ordering the main thread used to give for free.
///
/// Nothing here touches AppKit. Every UI update the engine makes leaves as an
/// event and `NativeEvents::emit` hops to the main queue itself, and the
/// clipboard and keystroke paths already run from the pipeline's tokio worker.
pub(crate) fn route<T: HotkeyTarget>(
    record_id: Option<u32>,
    recopy_id: Option<u32>,
    target: &T,
    event: GlobalHotKeyEvent,
) {
    if record_id == Some(event.id) {
        let target = target.clone();
        match event.state {
            HotKeyState::Pressed => run_off_main(Box::new(move || target.pressed())),
            HotKeyState::Released => run_off_main(Box::new(move || target.released())),
        }
    } else if recopy_id == Some(event.id) && matches!(event.state, HotKeyState::Pressed) {
        let target = target.clone();
        run_off_main(Box::new(move || target.recopy()));
    }
}

type HotkeyWork = Box<dyn FnOnce() + Send + 'static>;

/// The one thread every hotkey's engine call runs on, or `None` when the
/// process would not give us one.
fn worker() -> Option<&'static Sender<HotkeyWork>> {
    static WORKER: OnceLock<Option<Sender<HotkeyWork>>> = OnceLock::new();
    WORKER
        .get_or_init(|| {
            let (sender, receiver) = mpsc::channel::<HotkeyWork>();
            std::thread::Builder::new()
                .name("openflow-hotkeys".to_string())
                .spawn(move || {
                    for work in receiver {
                        work();
                    }
                })
                .ok()
                .map(|_| sender)
        })
        .as_ref()
}

/// Run `work` on the hotkey worker, or here when there is no worker to run it
/// on.
///
/// A machine that will not hand the process a thread is not a reason to drop
/// the take the user just spoke. The fallback is exactly what this did before:
/// a blocked caller, which is worse than a free one and far better than
/// silence.
fn run_off_main(work: HotkeyWork) {
    match worker() {
        Some(sender) => {
            if let Err(returned) = sender.send(work) {
                (returned.0)();
            }
        }
        None => work(),
    }
}

/// `global-hotkey` calls this from a Carbon application event handler, which
/// macOS runs on the main run loop. Nothing here touches AppKit or the engine;
/// both happen after the hop.
pub fn install_handler() {
    GlobalHotKeyEvent::set_event_handler(Some(|event: GlobalHotKeyEvent| {
        crate::events::on_main(move || {
            crate::app::with_app(|app| {
                let hotkeys = app.hotkeys().borrow();
                hotkeys.dispatch(app.engine(), event);
            });
        });
    }));
}

// ── The recorder field's string format ────────────────────

/// macOS virtual key codes for the keys that have no printable character, so a
/// recorder field can name them the way `parse_shortcut` expects.
const SPECIAL_KEYS: &[(u16, &str)] = &[
    (36, "Enter"),
    (48, "Tab"),
    (49, "Space"),
    (51, "Backspace"),
    (53, "Escape"),
    (122, "F1"),
    (120, "F2"),
    (99, "F3"),
    (118, "F4"),
    (96, "F5"),
    (97, "F6"),
    (98, "F7"),
    (100, "F8"),
    (101, "F9"),
    (109, "F10"),
    (103, "F11"),
    (111, "F12"),
];

/// The key half of a chord, as `parse_shortcut` spells it, or `None` when the
/// key is one it cannot express.
pub fn key_name(key_code: u16, characters: Option<&str>) -> Option<String> {
    if let Some((_, name)) = SPECIAL_KEYS.iter().find(|(code, _)| *code == key_code) {
        return Some((*name).to_string());
    }
    let first = characters?.chars().next()?;
    if first.is_ascii_alphanumeric() {
        Some(first.to_ascii_uppercase().to_string())
    } else {
        None
    }
}

/// The chord as a settings string. Modifier order is fixed so the same chord
/// always writes the same string, and it is the order the shipped defaults use
/// (`Ctrl+Shift+V`, `Option+V`).
///
/// A chord with no modifier is refused: a bare letter as a global hotkey would
/// swallow that letter in every app on the machine.
pub fn shortcut_string(
    control: bool,
    option: bool,
    shift: bool,
    command: bool,
    key: &str,
) -> Option<String> {
    if key.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    if control {
        parts.push("Ctrl");
    }
    if option {
        parts.push("Option");
    }
    if shift {
        parts.push("Shift");
    }
    if command {
        parts.push("Cmd");
    }
    if parts.is_empty() {
        return None;
    }
    parts.push(key);
    Some(parts.join("+"))
}

/// The chord a `HotKey` represents, for showing a saved binding in the recorder
/// field.
pub fn describe(shortcut: &HotKey) -> String {
    let mods = shortcut.mods;
    let key = code_name(shortcut.key).unwrap_or("?");
    // `HotKey::new` folds META into SUPER, so a chord built from "Cmd" carries
    // SUPER by the time it is stored. Reading only META would silently drop the
    // Command key from every description.
    shortcut_string(
        mods.contains(Modifiers::CONTROL),
        mods.contains(Modifiers::ALT),
        mods.contains(Modifiers::SHIFT),
        mods.contains(Modifiers::META) || mods.contains(Modifiers::SUPER),
        key,
    )
    .unwrap_or_else(|| key.to_string())
}

fn code_name(code: Code) -> Option<&'static str> {
    Some(match code {
        Code::Space => "Space",
        Code::Enter => "Enter",
        Code::Tab => "Tab",
        Code::Escape => "Escape",
        Code::Backspace => "Backspace",
        Code::KeyA => "A",
        Code::KeyB => "B",
        Code::KeyC => "C",
        Code::KeyD => "D",
        Code::KeyE => "E",
        Code::KeyF => "F",
        Code::KeyG => "G",
        Code::KeyH => "H",
        Code::KeyI => "I",
        Code::KeyJ => "J",
        Code::KeyK => "K",
        Code::KeyL => "L",
        Code::KeyM => "M",
        Code::KeyN => "N",
        Code::KeyO => "O",
        Code::KeyP => "P",
        Code::KeyQ => "Q",
        Code::KeyR => "R",
        Code::KeyS => "S",
        Code::KeyT => "T",
        Code::KeyU => "U",
        Code::KeyV => "V",
        Code::KeyW => "W",
        Code::KeyX => "X",
        Code::KeyY => "Y",
        Code::KeyZ => "Z",
        Code::Digit0 => "0",
        Code::Digit1 => "1",
        Code::Digit2 => "2",
        Code::Digit3 => "3",
        Code::Digit4 => "4",
        Code::Digit5 => "5",
        Code::Digit6 => "6",
        Code::Digit7 => "7",
        Code::Digit8 => "8",
        Code::Digit9 => "9",
        Code::F1 => "F1",
        Code::F2 => "F2",
        Code::F3 => "F3",
        Code::F4 => "F4",
        Code::F5 => "F5",
        Code::F6 => "F6",
        Code::F7 => "F7",
        Code::F8 => "F8",
        Code::F9 => "F9",
        Code::F10 => "F10",
        Code::F11 => "F11",
        Code::F12 => "F12",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::RecvTimeoutError;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    /// How long a stand-in engine call is allowed to hold the worker before it
    /// gives up. Long enough that a caller which ran it inline could not be
    /// mistaken for one that handed it off, short enough that the test still
    /// ends if the release channel is never signalled.
    const STANDIN_BLOCK: Duration = Duration::from_secs(5);

    #[derive(Clone)]
    struct Recorder {
        calls: Arc<Mutex<Vec<(&'static str, std::thread::ThreadId)>>>,
        /// Held by `released`, so the test decides when the release finishes.
        release: Arc<Mutex<Option<mpsc::Receiver<()>>>>,
        done: mpsc::Sender<&'static str>,
    }

    impl Recorder {
        fn new() -> (Self, mpsc::Receiver<&'static str>) {
            let (done, seen) = mpsc::channel();
            (
                Self {
                    calls: Arc::new(Mutex::new(Vec::new())),
                    release: Arc::new(Mutex::new(None)),
                    done,
                },
                seen,
            )
        }

        fn note(&self, what: &'static str) {
            self.calls
                .lock()
                .expect("call log")
                .push((what, std::thread::current().id()));
            let _ = self.done.send(what);
        }

        fn names(&self) -> Vec<&'static str> {
            self.calls
                .lock()
                .expect("call log")
                .iter()
                .map(|(name, _)| *name)
                .collect()
        }

        fn threads(&self) -> Vec<std::thread::ThreadId> {
            self.calls
                .lock()
                .expect("call log")
                .iter()
                .map(|(_, thread)| *thread)
                .collect()
        }
    }

    impl HotkeyTarget for Recorder {
        fn pressed(&self) {
            self.note("pressed");
        }

        fn released(&self) {
            if let Some(gate) = self.release.lock().expect("release gate").take() {
                match gate.recv_timeout(STANDIN_BLOCK) {
                    Ok(()) | Err(RecvTimeoutError::Disconnected) => {}
                    Err(RecvTimeoutError::Timeout) => {}
                }
            }
            self.note("released");
        }

        fn recopy(&self) {
            self.note("recopy");
        }
    }

    const RECORD: u32 = 11;
    const RECOPY: u32 = 22;

    fn event(id: u32, state: HotKeyState) -> GlobalHotKeyEvent {
        GlobalHotKeyEvent { id, state }
    }

    /// The release is the expensive one: `Engine::hotkey_released` waits on the
    /// audio thread while it resamples, gains and encodes the whole take, which
    /// is a quarter of a second for a take at the recording ceiling. `dispatch`
    /// runs on the main thread, so routing it there is the app not drawing for
    /// that long, at the exact moment the user has stopped talking and is
    /// looking at the overlay.
    #[test]
    fn releasing_the_key_does_not_hold_up_the_thread_that_delivered_it() {
        let (target, seen) = Recorder::new();
        let (unblock, gate) = mpsc::channel();
        *target.release.lock().expect("release gate") = Some(gate);

        let started = Instant::now();
        route(
            Some(RECORD),
            Some(RECOPY),
            &target,
            event(RECORD, HotKeyState::Released),
        );
        let handed_off = started.elapsed();

        assert!(
            handed_off < Duration::from_secs(1),
            "the release held the caller for {handed_off:?}; it must be handed off, \
             not run on the thread that delivered it"
        );
        // And it really did run: handing off is not dropping.
        unblock.send(()).expect("the release is still waiting");
        assert_eq!(
            seen.recv_timeout(STANDIN_BLOCK * 2),
            Ok("released"),
            "the release never reached the engine"
        );
        assert_eq!(target.names(), vec!["released"]);
        assert_ne!(
            target.threads()[0],
            std::thread::current().id(),
            "the release ran on the calling thread"
        );
    }

    /// Off the main thread, but not out of order. A press that lands while the
    /// previous release is still encoding must still reach the engine second,
    /// or the engine sees a capture starting before the one it replaces has
    /// ended and the overlay publishes the two states backwards.
    #[test]
    fn hotkeys_reach_the_engine_in_the_order_they_arrived() {
        let (target, seen) = Recorder::new();
        let (unblock, gate) = mpsc::channel();
        *target.release.lock().expect("release gate") = Some(gate);

        route(
            Some(RECORD),
            Some(RECOPY),
            &target,
            event(RECORD, HotKeyState::Released),
        );
        route(
            Some(RECORD),
            Some(RECOPY),
            &target,
            event(RECORD, HotKeyState::Pressed),
        );
        route(
            Some(RECORD),
            Some(RECOPY),
            &target,
            event(RECOPY, HotKeyState::Pressed),
        );
        // Everything above queued behind a release that has not finished yet.
        // A caller that ran the release inline has already given up waiting by
        // now; let that be the thread assertion's failure to report, not this
        // line's.
        let _ = unblock.send(());

        for expected in ["released", "pressed", "recopy"] {
            assert_eq!(
                seen.recv_timeout(STANDIN_BLOCK * 2),
                Ok(expected),
                "expected {expected} next"
            );
        }
        assert_eq!(target.names(), vec!["released", "pressed", "recopy"]);
        let threads = target.threads();
        assert_ne!(
            threads[0],
            std::thread::current().id(),
            "the hotkey calls ran on the thread that delivered them"
        );
        assert!(
            threads.windows(2).all(|pair| pair[0] == pair[1]),
            "the hotkey calls were spread over several threads, so nothing keeps them in order"
        );
    }

    /// A key press that is neither binding is not the engine's business.
    #[test]
    fn an_unbound_chord_and_a_recopy_release_reach_nothing() {
        let (target, seen) = Recorder::new();
        route(
            Some(RECORD),
            Some(RECOPY),
            &target,
            event(99, HotKeyState::Pressed),
        );
        route(
            Some(RECORD),
            Some(RECOPY),
            &target,
            event(RECOPY, HotKeyState::Released),
        );
        route(None, None, &target, event(RECORD, HotKeyState::Released));
        assert_eq!(
            seen.recv_timeout(Duration::from_millis(500)),
            Err(RecvTimeoutError::Timeout),
            "an event for no binding of ours reached the engine"
        );
        assert!(target.names().is_empty());
    }

    /// The recorder writes settings strings, so everything it can produce has
    /// to survive `parse_shortcut` unchanged. A format the parser rejects would
    /// silently fall back to the default binding.
    #[test]
    fn every_recorded_chord_parses_back_to_itself() {
        let cases = [
            (false, true, false, false, "V", "Option+V"),
            (true, false, true, false, "V", "Ctrl+Shift+V"),
            (false, false, true, true, "K", "Shift+Cmd+K"),
            (true, true, true, true, "F5", "Ctrl+Option+Shift+Cmd+F5"),
            (false, false, false, true, "Space", "Cmd+Space"),
        ];
        for (control, option, shift, command, key, expected) in cases {
            let recorded = shortcut_string(control, option, shift, command, key)
                .unwrap_or_else(|| panic!("{expected} should be recordable"));
            assert_eq!(recorded, expected);
            let parsed = hotkey::parse_shortcut(&recorded)
                .unwrap_or_else(|error| panic!("{recorded} should parse: {error}"));
            assert_eq!(describe(&parsed), expected, "{recorded} should round-trip");
        }

        // The parser is order-insensitive, so a binding a user saved in the old
        // web settings screen still resolves to the same chord; only the
        // spelling the recorder writes back is fixed.
        assert_eq!(
            hotkey::parse_shortcut("Cmd+Shift+K"),
            hotkey::parse_shortcut("Shift+Cmd+K")
        );
    }

    /// The two shipped defaults are the strings this function has to be able to
    /// produce, or a user who re-records the default gets a different string.
    #[test]
    fn the_shipped_defaults_are_recordable_strings() {
        for (action, default) in hotkey::HOTKEY_DEFAULTS {
            let parsed = hotkey::parse_shortcut(default)
                .unwrap_or_else(|_| panic!("the {action} default must parse"));
            assert_eq!(
                describe(&parsed),
                *default,
                "the recorder must spell the {action} default exactly as shipped"
            );
        }
    }

    /// A bare key would take that key away from every app on the machine.
    #[test]
    fn a_chord_without_a_modifier_is_refused() {
        assert_eq!(shortcut_string(false, false, false, false, "V"), None);
        assert_eq!(shortcut_string(false, true, false, false, ""), None);
    }

    #[test]
    fn key_names_come_from_the_keycode_or_the_character() {
        assert_eq!(key_name(49, Some(" ")).as_deref(), Some("Space"));
        assert_eq!(key_name(53, Some("\u{1b}")).as_deref(), Some("Escape"));
        assert_eq!(key_name(96, None).as_deref(), Some("F5"));
        assert_eq!(key_name(9, Some("v")).as_deref(), Some("V"));
        assert_eq!(key_name(9, Some("7")).as_deref(), Some("7"));
        // A dead key or a punctuation mark `parse_shortcut` has no name for.
        assert_eq!(key_name(24, Some("=")), None);
        assert_eq!(key_name(9, None), None);
    }
}
