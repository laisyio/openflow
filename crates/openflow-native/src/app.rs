//! The NSApplication bootstrap and the one piece of shared main-thread state.
//!
//! Ordering matters here and is deliberate:
//!
//! 1. The single-instance lock is taken before anything else, so a second copy
//!    exits before it can register a hotkey or write to the database.
//! 2. `NSApplication` is created and put in accessory mode (the `LSUIElement`
//!    behaviour: no Dock icon, no menu bar) before any window exists.
//! 3. The engine is built in `applicationDidFinishLaunching:`, because
//!    `Engine::new` opens the database and the keychain and that must not
//!    happen before the app has an event loop to report failures on.
//! 4. The tokio runtime is owned *here*, in a static that outlives the run
//!    loop, and the engine only gets a spawner over its handle. If the engine
//!    owned the runtime, a transcription task holding the last `Arc<Engine>`
//!    would drop that runtime from one of its own worker threads, which tokio
//!    turns into a panic.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, OnceLock};

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAppearance, NSAppearanceNameAqua, NSAppearanceNameDarkAqua, NSApplication,
    NSApplicationActivationPolicy, NSApplicationDelegate,
};
use objc2_foundation::{NSNotification, NSObject, NSObjectProtocol};

use openflow_core::engine::{Engine, EngineEvent, EngineEvents, Failure, RecordingState, Spawner};

use crate::events::{NativeEvents, PreviewGate};
use crate::hotkeys::Hotkeys;
use crate::instance::InstanceLock;
use crate::overlay::{Outcome, Overlay};
use crate::tray::Tray;
use crate::tts_player::TtsPlayer;
use crate::ui::main_window::MainWindow;
use crate::ui::onboarding::OnboardingWindow;
use crate::ui::settings::SettingsPage;

/// The bundle identifier the Tauri build uses, so both read one database and
/// one set of keychain items.
pub const BUNDLE_ID: &str = "io.laisy.openflow";

/// Owned for the life of the process. Never dropped, which is the point: the
/// engine spawns onto its handle and a task may outlive the run loop.
static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

thread_local! {
    /// The live app, reachable from every main-thread callback. `None` before
    /// `applicationDidFinishLaunching:` and after a failed start.
    static APP: RefCell<Option<Rc<App>>> = const { RefCell::new(None) };
}

/// Run `body` with the app if it exists. The `Rc` is cloned out of the cell
/// first, so a callback that re-enters (a menu action that emits an event, say)
/// does not borrow the cell twice.
pub fn with_app<R>(body: impl FnOnce(&Rc<App>) -> R) -> Option<R> {
    let app = APP.with(|slot| slot.borrow().clone());
    app.as_ref().map(body)
}

/// Put the app in the Dock while a window is open and take it out once they are
/// all closed, leaving the status item as the way back in either way.
///
/// The pill and the status item are not windows for this purpose. An app that
/// jumped into the Dock every time someone spoke would be worse than one that
/// never appeared there at all.
///
/// `presenting` is set by the caller that is about to show a window: the window
/// is not visible yet at that moment, so counting alone would say no.
///
/// `LSUIElement` in the bundle still decides how the app *launches* -- as an
/// accessory, with no Dock icon and no menu bar until something asks for one.
pub fn refresh_dock_presence(presenting: bool) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let visible = presenting || with_app(|app| app.has_visible_window()).unwrap_or(false);
    let policy = if visible {
        NSApplicationActivationPolicy::Regular
    } else {
        NSApplicationActivationPolicy::Accessory
    };
    let ns_app = NSApplication::sharedApplication(mtm);
    if ns_app.activationPolicy() == policy {
        return;
    }
    crate::trace!("dock presence={}", visible);
    ns_app.setActivationPolicy(policy);
}

/// Everything the main thread owns.
pub struct App {
    engine: Arc<Engine>,
    overlay: Overlay,
    tray: Tray,
    hotkeys: RefCell<Hotkeys>,
    tts: TtsPlayer,
    // One slot per window, each built on first use and kept: closing hides,
    // so reopening from the menu bar is instant and no state is rebuilt.
    //
    // Two slots, not five. Dictate, History, Plugins and Settings are pages
    // of `main` now rather than windows of their own. The wizard keeps its own
    // window because it is presented as a sheet on `main`, and a sheet is a
    // window.
    main: RefCell<Option<Retained<MainWindow>>>,
    onboarding: RefCell<Option<Retained<OnboardingWindow>>>,
    mtm: MainThreadMarker,
}

impl App {
    pub fn engine(&self) -> &Arc<Engine> {
        &self.engine
    }

    pub fn overlay(&self) -> &Overlay {
        &self.overlay
    }

    pub fn tray(&self) -> &Tray {
        &self.tray
    }

    pub fn tts(&self) -> &TtsPlayer {
        &self.tts
    }

    pub fn hotkeys(&self) -> &RefCell<Hotkeys> {
        &self.hotkeys
    }

    /// Run `body` against the settings page if the main window has been built.
    /// Nothing to do when it has not: whatever the result was, `reload` will
    /// read it back the next time the page is shown.
    pub fn with_settings<R>(&self, body: impl FnOnce(&SettingsPage) -> R) -> Option<R> {
        self.with_main(|window| body(&window.settings()))
    }

    /// Run `body` against the onboarding window if it has been built.
    pub fn with_onboarding<R>(&self, body: impl FnOnce(&OnboardingWindow) -> R) -> Option<R> {
        let window = self.onboarding.borrow().clone();
        window.as_deref().map(body)
    }

    /// True while either window is on screen.
    ///
    /// Asks the windows rather than counting opens and closes: they are hidden
    /// rather than closed and can be ordered out by AppKit itself, and a
    /// counter that drifted would strand the Dock icon with nothing behind it.
    pub fn has_visible_window(&self) -> bool {
        fn visible<W>(
            slot: &RefCell<Option<Retained<W>>>,
            is_visible: impl Fn(&W) -> bool,
        ) -> bool {
            slot.borrow().as_deref().is_some_and(is_visible)
        }
        visible(&self.main, |w| w.is_visible()) || visible(&self.onboarding, |w| w.is_visible())
    }

    /// Run `body` against the main window if it has been built.
    pub fn with_main<R>(&self, body: impl FnOnce(&MainWindow) -> R) -> Option<R> {
        let window = self.main.borrow().clone();
        window.as_deref().map(body)
    }

    /// Show the main window, building it on first use, on `page`. `None` leaves
    /// it on whichever page the user left it.
    pub fn show_main(self: &Rc<Self>, page: Option<&str>) {
        // The borrow ends before the window is touched, for the reason spelled
        // out on `show_settings`: building and presenting run AppKit code that
        // can call straight back in here.
        let window = {
            let mut slot = self.main.borrow_mut();
            slot.get_or_insert_with(|| MainWindow::new(self, self.mtm))
                .clone()
        };
        if let Some(page) = page {
            window.show_named(page);
        }
        window.reload();
        window.present();
    }

    /// Show the setup wizard as a sheet on the main window.
    ///
    /// The main window is built and presented first, because a sheet needs
    /// something to hang from -- including on a first launch, where the wizard
    /// is the whole reason the app opened. That is not a workaround: setup now
    /// happens over the workspace it is setting up.
    pub fn show_onboarding(self: &Rc<Self>) {
        self.show_main(None);
        // Same narrowed borrow as `show_main`: building, reloading and
        // presenting all run AppKit code that can call back in here.
        let sheet = {
            let mut slot = self.onboarding.borrow_mut();
            slot.get_or_insert_with(|| OnboardingWindow::new(self, self.mtm))
                .clone()
        };
        sheet.reload();
        if let Some(parent) = self.with_main(|window| window.window()) {
            sheet.present_on(&parent);
        }
    }

    /// Apply one engine event. Always called on the main thread, after the
    /// `dispatch2` hop in [`crate::events`], so it is free to touch windows and
    /// to read the engine back.
    pub fn handle_event(self: &Rc<Self>, event: EngineEvent) {
        match event {
            EngineEvent::RecordingState(state) => {
                // `Formatting` is never emitted by the pipeline; treat anything
                // that is not Recording or Transcribing as the resting state
                // rather than inventing a fourth pill.
                self.overlay.set_state(state);
                // Starting a take answers the last one: the user has seen the
                // failure and is trying again, and a menu bar still reporting
                // it would be reporting the wrong take.
                if state == RecordingState::Recording {
                    self.clear_problem();
                }
                self.tray.set_status(state);
                self.with_main(|window| window.set_state(state));
            }
            EngineEvent::TranscriptionResult(transcription) => {
                let text = transcription
                    .formatted_text
                    .as_deref()
                    .unwrap_or(&transcription.raw_text);
                self.notify("OpenFlow", &first_line(text));
                // Deliberately not clearing the standing problem here. A take
                // can arrive *with* one -- "not saved to history" is the take
                // being handed over and the write having failed -- and this
                // event is emitted after the warning that says so. Clearing
                // here wiped the warning with the very take it was about. The
                // capture that starts the next take clears it instead, which is
                // also the point at which it stops being about this one.
                self.overlay.show_outcome(Outcome::Done);
                // The web screen's result button, which says the text is on the
                // clipboard because the pipeline has just put it there.
                self.with_main(|window| window.dictate().set_last(text, "Copied to clipboard"));
            }
            // The pill owns this one entirely: a reading of a recording still
            // in progress is never notified, saved or typed.
            EngineEvent::TranscriptionPartial(partial) => {
                self.overlay.set_partial(&partial.text, partial.held)
            }
            // A take that arrived with something wrong with it. The text is
            // the user's and has been handed over; the problem still stands,
            // and stands in the same place a failure would.
            EngineEvent::TranscriptionWarning(warning) => self.show_problem(warning),
            // Report it and stop there. The engine decides when the pill rests,
            // through `emit_idle_if_quiescent`, which only says "idle" once no
            // capture is running and no job is left. Forcing idle here would
            // blank the pill mid-recording whenever a previous take failed
            // while the user was already holding the key down again.
            EngineEvent::TranscriptionError(error) => {
                self.show_problem(error);
                self.overlay.show_outcome(Outcome::Error);
            }
            EngineEvent::RecopySuccess(message) => self.notify("OpenFlow", &message),
            // The recents menu and the main window are the two surfaces
            // holding a copy of the list, and both re-read it here. The page
            // re-runs its own search, so a filtered list stays filtered, and
            // Dictate re-reads the newest row for its result card.
            EngineEvent::HistoryChanged => {
                self.tray.rebuild(&self.engine);
                self.with_main(|window| {
                    window.history().load();
                    window.dictate().load();
                });
            }
            EngineEvent::TtsStarted(started) => self.tts.started(&started),
            EngineEvent::TtsChunk(chunk) => self.tts.chunk(&chunk),
            // Both of these are written against the request id, never
            // unconditionally: with one id per preview, a stream that was
            // cancelled reports back after the next preview is already on
            // screen, and an unguarded write would replace the live status with
            // the dead stream's message.
            EngineEvent::TtsFinished(result) => {
                self.tts.finished(&result);
                // A player thread that failed to open the device or decode the
                // clip has nowhere else to report; surface it here.
                let message = self
                    .tts
                    .last_error()
                    .unwrap_or_else(|| "Playing the preview.".to_string());
                self.with_settings(|window| {
                    window.set_voice_status_for(&result.request_id, &message)
                });
            }
            EngineEvent::TtsError(error) => {
                self.tts.failed(&error);
                self.with_settings(|window| {
                    window.set_voice_status_for(&error.request_id, &error.error)
                });
            }
            // The local runner pushes its own state; nothing here polls the
            // supervisor. The window may not be built, in which case there is
            // nothing to draw and `reload` reads the current state when it
            // opens.
            EngineEvent::RunnerState(status) => {
                crate::trace!("runner {} {}", status.phase.as_str(), status.detail);
                self.with_settings(|window| window.set_runner_state(&status));
            }
            EngineEvent::Navigate(target) => match target.as_str() {
                "quit" => {
                    let app = NSApplication::sharedApplication(self.mtm);
                    app.terminate(None);
                }
                "onboarding" => self.show_onboarding(),
                "main" => self.show_main(None),
                // Everything else is a page or a group inside one, and the
                // window knows which: "history" and "plugins" are pages,
                // "voice" and "privacy" are groups of the Settings page.
                target => self.show_main(Some(target)),
            },
        }
    }

    /// A one-line status message. There is no notification centre entitlement in
    /// this build, so the status item's tooltip carries it: visible, free, and
    /// it cannot steal focus from whatever the user is dictating into.
    ///
    /// For anything that went wrong use [`App::show_problem`] instead. A
    /// tooltip written here lasts until the next thing writes one, and the
    /// settling that follows every take is one of those. It is also refused
    /// outright while a failure is standing.
    fn notify(&self, title: &str, body: &str) {
        self.tray.set_tooltip(&format!("{}: {}", title, body));
    }

    /// Put a failure where the user can still find it a minute later, and next
    /// to the thing that answers it.
    ///
    /// Three surfaces, because a `LSUIElement` app has no one place a user is
    /// certain to be looking: the menu bar line (always there), the item under
    /// it that opens the group in Settings that answers this failure, and the
    /// result card on Dictate for whoever has the window open.
    fn show_problem(self: &Rc<Self>, problem: Failure) {
        let target = problem.remedy.map(|remedy| remedy.target());
        let message = problem.message.clone();
        if self.tray.set_problem(Some(problem)) {
            self.tray.rebuild(&self.engine);
        }
        self.with_main(|window| window.dictate().set_problem(&message, target));
    }

    /// Take the standing failure down, once it has been answered.
    fn clear_problem(self: &Rc<Self>) {
        if self.tray.set_problem(None) {
            self.tray.rebuild(&self.engine);
        }
        self.with_main(|window| window.dictate().clear_problem());
    }
}

fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    let preview: String = line.chars().take(60).collect();
    if line.chars().count() > 60 {
        format!("{}...", preview)
    } else {
        preview
    }
}

/// Force the app's windows light or dark, or follow the system when the
/// setting holds neither. `Settings::theme` already filters anything that is
/// not "dark" or "light" to `None`, which is what "follow the system" is.
pub fn apply_theme(theme: Option<&str>, mtm: MainThreadMarker) {
    let appearance = match theme {
        Some("dark") => NSAppearance::appearanceNamed(unsafe { NSAppearanceNameDarkAqua }),
        Some("light") => NSAppearance::appearanceNamed(unsafe { NSAppearanceNameAqua }),
        _ => None,
    };
    NSApplication::sharedApplication(mtm).setAppearance(appearance.as_deref());
}

/// `~/Library/Application Support/io.laisy.openflow`, the exact directory
/// Tauri's `app_data_dir()` resolves to, so the two builds share one database.
///
/// `OPENFLOW_APP_DIR` overrides it. That exists for `--transcribe`: measuring
/// the local runner should not have to point at the settings, history and
/// keychain of the person running the measurement.
pub fn default_app_dir() -> Result<PathBuf, String> {
    if let Some(override_dir) = std::env::var_os("OPENFLOW_APP_DIR") {
        return Ok(PathBuf::from(override_dir));
    }
    let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
    Ok(PathBuf::from(home)
        .join("Library")
        .join("Application Support")
        .join(BUNDLE_ID))
}

/// Build the engine on the process-wide runtime.
pub fn build_engine(app_dir: PathBuf) -> Result<(Arc<Engine>, Arc<PreviewGate>), String> {
    let preview = Arc::new(PreviewGate::default());
    let events: Arc<dyn EngineEvents> = Arc::new(NativeEvents::new(Arc::clone(&preview)));
    let engine = build_engine_with(app_dir, events)?;
    Ok((engine, preview))
}

/// The same, with a caller-supplied sink. `--transcribe` uses it to print
/// runner progress to stderr instead of hopping to a main thread that has no
/// windows on it.
pub fn build_engine_with(
    app_dir: PathBuf,
    events: Arc<dyn EngineEvents>,
) -> Result<Arc<Engine>, String> {
    let runtime = match RUNTIME.get() {
        Some(runtime) => runtime,
        None => {
            let built = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|error| format!("Could not start the async runtime: {}", error))?;
            RUNTIME.get_or_init(|| built)
        }
    };
    let handle = runtime.handle().clone();
    let spawn: Spawner = Box::new(move |future| {
        handle.spawn(future);
    });

    Engine::new(app_dir, events, spawn)
}

// ── App delegate ──────────────────────────────────────────

pub struct DelegateIvars {
    app_dir: PathBuf,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and the class holds no
    // Drop-relevant state beyond its ivars.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "OpenFlowAppDelegate"]
    #[ivars = DelegateIvars]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl NSApplicationDelegate for Delegate {
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish_launching(&self, _notification: &NSNotification) {
            let mtm = MainThreadMarker::from(self);
            let app_dir = self.ivars().app_dir.clone();
            if let Err(error) = start(app_dir, mtm) {
                eprintln!("OpenFlow could not start: {}", error);
                NSApplication::sharedApplication(mtm).terminate(None);
            }
        }

        /// The quit path. A local runner holding 2.5 GB of model weights must
        /// not survive the app that started it, and `Drop` on the engine is not
        /// a promise anyone can keep here: the engine is an `Arc` a tokio task
        /// may still hold when the run loop ends. So the sidecar is killed
        /// explicitly, on the one callback AppKit guarantees before exit.
        #[unsafe(method(applicationWillTerminate:))]
        fn will_terminate(&self, _notification: &NSNotification) {
            crate::trace!("terminate");
            with_app(|app| app.engine().runner().stop());
        }

        /// Clicking the app in the Dock or Launchpad on an accessory app lands
        /// here; the Tauri build opens its window on the same signal.
        #[unsafe(method(applicationShouldHandleReopen:hasVisibleWindows:))]
        fn should_handle_reopen(&self, _sender: &NSApplication, _has_visible: bool) -> bool {
            crate::trace!("reopen");
            // Same present path as the tray, so the reopen click brings the
            // window to the active Space and the app forward with it.
            with_app(|app| app.show_main(None));
            true
        }
    }
);

impl Delegate {
    fn new(app_dir: PathBuf, mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(DelegateIvars { app_dir });
        unsafe { msg_send![super(this), init] }
    }
}

/// Everything that needs a live engine, in the order the pieces depend on
/// each other.
fn start(app_dir: PathBuf, mtm: MainThreadMarker) -> Result<(), String> {
    let (engine, _preview) = build_engine(app_dir)?;

    // The handlers go on before the things that raise events exist. Both are
    // process-wide callbacks that hop to the main thread and then ask
    // `with_app`, which answers `None` until the app is in its slot below, so
    // an event raised during construction is dropped rather than reaching a
    // half-built app. Installed the other way round, a menu click or a hotkey
    // press between `Tray::new` and `install_handler` would have no handler at
    // all and be lost with no trace of why.
    crate::hotkeys::install_handler();
    crate::tray::install_handler();

    let overlay = Overlay::new(&engine, mtm);
    let tray = Tray::new(&engine)?;
    let hotkeys = Hotkeys::new(engine.settings())?;
    let tts = TtsPlayer::new(_preview);

    let app = Rc::new(App {
        engine,
        overlay,
        tray,
        hotkeys: RefCell::new(hotkeys),
        tts,
        main: RefCell::new(None),
        onboarding: RefCell::new(None),
        mtm,
    });
    APP.with(|slot| *slot.borrow_mut() = Some(Rc::clone(&app)));

    // Key equivalents only exist if there is a main menu to route them
    // through, even though an accessory app never draws one.
    crate::menu::install(mtm);

    apply_theme(app.engine.settings().theme().as_deref(), mtm);
    app.overlay.apply_visibility_setting();
    // A fresh install has no provider saved, and setup is what it needs first.
    // Otherwise open the main window, because the Tauri build's window is
    // `"visible": true` in `tauri.conf.json` and so puts one up on every
    // launch, on its main screen. An accessory app that draws nothing but a
    // menu bar item looks like a launch that failed, which is what a smoke run
    // reported. Until this window existed the stand-in was Settings, which
    // opened the app on its own preferences; now there is a screen to land on.
    if !app.engine.settings().onboarding_complete() {
        app.show_onboarding();
    } else {
        app.show_main(None);
    }
    Ok(())
}

/// Construct the engine against a throwaway app directory, prove it comes up,
/// and exit. Registers no hotkey, opens no window, and touches no keychain: a
/// fresh directory has no plaintext secrets to migrate, which is the only
/// startup path that reads one.
fn self_check() -> i32 {
    let dir = std::env::temp_dir().join(format!("openflow-self-check-{}", std::process::id()));
    let result = build_engine(dir.clone()).and_then(|(engine, _)| {
        let position = engine.settings().overlay_position();
        let record = engine.settings().shortcut("record")?;
        let recopy = engine.settings().shortcut("recopy")?;
        Ok((position, record, recopy))
    });
    let _ = std::fs::remove_dir_all(&dir);
    match result {
        Ok((position, record, recopy)) => {
            println!(
                "ok engine=up overlay_position={} record={:?} recopy={:?}",
                position,
                record.id(),
                recopy.id()
            );
            0
        }
        Err(error) => {
            eprintln!("self-check failed: {}", error);
            1
        }
    }
}

/// Transcribe one wav with the saved settings, print the text and the time it
/// took, and exit. No window, no hotkey, no tray.
///
/// This is how the local runner gets measured: the GUI cannot be launched from
/// a shell without breaking its TCC grants, and a dictation cannot be timed
/// from the outside. Only the transcription leg runs -- no cleanup pass, no
/// history row, and nothing put on the clipboard or typed into whatever is
/// focused when a measurement happens to run.
fn transcribe_file(path: &str) -> i32 {
    /// Runner progress on stderr, so a `--transcribe` that has to install or
    /// load a model says what it is doing instead of sitting silent.
    struct StderrEvents;
    impl EngineEvents for StderrEvents {
        fn emit(&self, event: EngineEvent) -> Result<(), String> {
            if let EngineEvent::RunnerState(status) = event {
                // The port is part of the line because it arrives *after* the
                // spawn -- the child picks it and prints it -- so a start
                // reports twice, and without the port the second line reads
                // like a second sidecar.
                match status.port {
                    Some(port) => eprintln!(
                        "runner {} :{}: {}",
                        status.phase.as_str(),
                        port,
                        status.detail
                    ),
                    None => eprintln!("runner {}: {}", status.phase.as_str(), status.detail),
                }
            }
            Ok(())
        }
    }

    let wav = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("Could not read {}: {}", path, error);
            return 1;
        }
    };
    let result = default_app_dir()
        .and_then(|dir| {
            std::fs::create_dir_all(&dir)
                .map_err(|error| format!("Could not create {}: {}", dir.display(), error))?;
            build_engine_with(dir, Arc::new(StderrEvents))
        })
        .and_then(|engine| {
            let runtime = RUNTIME.get().ok_or("The runtime did not start")?;
            let outcome = runtime.block_on(engine.transcribe_wav(wav));
            // Kill the sidecar before the process leaves, since there is no
            // AppKit termination callback on this path.
            engine.runner().stop();
            outcome
        });
    match result {
        Ok((text, elapsed)) => {
            println!("{}", text);
            eprintln!("{} ms", elapsed.as_millis());
            0
        }
        Err(error) => {
            eprintln!("transcribe failed: {}", error);
            1
        }
    }
}

pub fn main() {
    // Before anything AppKit, and before the instance lock: `--version` is what
    // a bug report and the bundle script both ask, and neither wants a second
    // copy of the app refusing to start or a keychain prompt on the way to one
    // line of text.
    if std::env::args().any(|argument| argument == "--version") {
        println!("{}", crate::version::long());
        std::process::exit(0);
    }
    if std::env::args().any(|argument| argument == "--self-check") {
        std::process::exit(self_check());
    }
    let arguments: Vec<String> = std::env::args().collect();
    if let Some(index) = arguments.iter().position(|a| a == "--transcribe") {
        let Some(path) = arguments.get(index + 1) else {
            eprintln!("--transcribe needs the path to a .wav file");
            std::process::exit(2);
        };
        std::process::exit(transcribe_file(path));
    }

    let app_dir = match default_app_dir() {
        Ok(dir) => dir,
        Err(error) => {
            eprintln!(
                "OpenFlow could not find its application directory: {}",
                error
            );
            std::process::exit(1);
        }
    };
    if let Err(error) = std::fs::create_dir_all(&app_dir) {
        eprintln!("OpenFlow could not create {}: {}", app_dir.display(), error);
        std::process::exit(1);
    }

    // Held for the life of the process. A second copy exits here, before it can
    // register a hotkey or open the database.
    let lock = match InstanceLock::acquire(&app_dir) {
        Ok(lock) => lock,
        Err(error) => {
            eprintln!("{}", error);
            std::process::exit(0);
        }
    };

    let mtm = MainThreadMarker::new().expect("main() runs on the main thread");
    let ns_app = NSApplication::sharedApplication(mtm);
    ns_app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let delegate = Delegate::new(app_dir, mtm);
    ns_app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));

    ns_app.run();
    drop(lock);
}

/// Run a future on the process runtime. The settings window uses it for the
/// two calls that are async in core: fetching models and streaming a preview.
pub fn spawn(future: impl std::future::Future<Output = ()> + Send + 'static) {
    if let Some(runtime) = RUNTIME.get() {
        runtime.spawn(future);
    }
}
