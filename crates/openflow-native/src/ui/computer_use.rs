//! Opt-in, one-command-at-a-time computer use. AX handles never leave this thread.
use crate::computer_access::{self, Snapshot};
use crate::ui;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAccessibility, NSApplicationActivationOptions, NSBackingStoreType, NSButton,
    NSControlStateValueOff, NSControlStateValueOn, NSRunningApplication, NSSecureTextField,
    NSSwitch, NSTextField, NSTextView, NSView, NSWindow, NSWindowDelegate, NSWindowStyleMask,
    NSWorkspace,
};
use objc2_foundation::{NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};
use openflow_core::{
    computer_use::{self, Proposal, ReviewSession},
    engine::Engine,
};
use std::cell::{Cell, RefCell};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub struct Ivars {
    engine: Arc<Engine>,
    window: Retained<NSWindow>,
    key: Retained<NSSecureTextField>,
    consent: Retained<NSSwitch>,
    command: Retained<NSTextField>,
    target: Retained<NSTextField>,
    status: Retained<NSTextView>,
    record: Retained<NSButton>,
    review: Retained<NSButton>,
    confirm: Retained<NSButton>,
    pid: Cell<i32>,
    application: RefCell<String>,
    session: RefCell<ReviewSession<(Snapshot, Proposal, String)>>,
    stop: RefCell<Option<CancellationToken>>,
    snapshot: RefCell<Option<Snapshot>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = Ivars]
    pub struct ComputerUseWindow;

    unsafe impl NSObjectProtocol for ComputerUseWindow {}
    unsafe impl NSWindowDelegate for ComputerUseWindow {
        #[unsafe(method(windowShouldClose:))]
        fn window_should_close(&self, _sender: &NSWindow) -> bool {
            self.reset();
            self.ivars().consent.setState(NSControlStateValueOff);
            self.ivars().key.setStringValue(&NSString::from_str(""));
            self.ivars().command.setStringValue(&NSString::from_str(""));
            self.message("Session ended. No command will execute.");
            ui::dismiss_window(&self.ivars().window, "computer-use");
            false
        }
    }
    impl ComputerUseWindow {
        #[unsafe(method(saveKey:))]
        fn save_key(&self, _sender: &AnyObject) {
            self.reset();
            let value = self.ivars().key.stringValue().to_string();
            match self.ivars().engine.settings().set("jev_api_key", value.trim()) {
                Ok(()) => {
                    self.ivars().key.setStringValue(&NSString::from_str(""));
                    self.message(if value.trim().is_empty() { "TypeSafe key removed." } else { "TypeSafe key saved in Keychain." });
                }
                Err(_) => self.message("Could not save the key in Keychain. Please try again."),
            }
        }
        #[unsafe(method(consentChanged:))]
        fn consent_changed(&self, _sender: &AnyObject) {
            self.reset();
            self.message("Choose one command. Every action requires your confirmation.");
        }
        #[unsafe(method(cancelCommand:))]
        fn cancel_command(&self, _sender: &AnyObject) {
            self.reset();
            self.message("Cancelled. No action will execute.");
        }
        #[unsafe(method(reviewCommand:))]
        fn review_command(&self, _sender: &AnyObject) { self.review(); }
        #[unsafe(method(recordCommand:))]
        fn record_command(&self, _sender: &AnyObject) { self.record(); }
        #[unsafe(method(confirmCommand:))]
        fn confirm_command(&self, _sender: &AnyObject) {
            // Consume before any call into another app: a double click cannot replay it.
            let pending = self.ivars().session.borrow_mut().take();
            self.ivars().confirm.setEnabled(false);
            if !self.allowed() { return; }
            if let Some((snapshot, proposal, command)) = pending {
                if command != self.ivars().command.stringValue().to_string() {
                    self.message("The command changed. Review it again before confirming.");
                    return;
                }
                let pid=snapshot.pid;
                match snapshot.execute(&proposal) {
                    Ok(()) => {
                        self.message("Action sent to the target app. Check its result before your next command.");
                        if let Some(app)=NSRunningApplication::runningApplicationWithProcessIdentifier(pid) {
                            app.activateWithOptions(NSApplicationActivationOptions::empty());
                        }
                    },
                    Err(error) => self.message(&error),
                }
            }
        }
    }
);

fn frame(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}

impl ComputerUseWindow {
    pub fn new(engine: Arc<Engine>, mtm: MainThreadMarker) -> Retained<Self> {
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                frame(0., 0., 640., 640.),
                NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        window.setTitle(&NSString::from_str("Computer use · Jev preview"));
        unsafe {
            window.setReleasedWhenClosed(false);
        }
        window.center();
        let Controls {
            view,
            key,
            save,
            consent,
            command,
            target,
            status,
            record,
            review,
            cancel,
            confirm,
        } = controls(mtm);
        window.setContentView(Some(&view));
        let this = Self::alloc(mtm).set_ivars(Ivars {
            engine,
            window,
            key,
            consent,
            command,
            target,
            status,
            record,
            review,
            confirm,
            pid: Cell::new(0),
            application: RefCell::new(String::new()),
            session: RefCell::new(ReviewSession::default()),
            stop: RefCell::new(None),
            snapshot: RefCell::new(None),
        });
        let this: Retained<Self> = unsafe { msg_send![super(this), init] };
        this.ivars()
            .window
            .setDelegate(Some(ProtocolObject::from_ref(&*this)));
        ui::wire(&save, &this, sel!(saveKey:));
        ui::wire(&this.ivars().consent, &this, sel!(consentChanged:));
        ui::wire(&this.ivars().review, &this, sel!(reviewCommand:));
        ui::wire(&this.ivars().record, &this, sel!(recordCommand:));
        ui::wire(&this.ivars().confirm, &this, sel!(confirmCommand:));
        ui::wire(&cancel, &this, sel!(cancelCommand:));
        this.message("Add your TypeSafe key, enable session sharing, then type or record one command. Voice stops automatically after 15 seconds.");
        this
    }
}

struct Controls {
    view: Retained<NSView>,
    key: Retained<NSSecureTextField>,
    save: Retained<NSButton>,
    consent: Retained<NSSwitch>,
    command: Retained<NSTextField>,
    target: Retained<NSTextField>,
    status: Retained<NSTextView>,
    record: Retained<NSButton>,
    review: Retained<NSButton>,
    cancel: Retained<NSButton>,
    confirm: Retained<NSButton>,
}

fn controls(mtm: MainThreadMarker) -> Controls {
    let view = NSView::initWithFrame(NSView::alloc(mtm), frame(0., 0., 640., 640.));
    let heading = ui::note(
        mtm,
        "Say it. Review it. Do it.",
        frame(24., 583., 592., 38.),
    );
    heading.setFont(Some(&ui::editorial_font(26.)));
    view.addSubview(&heading);
    let intro = ui::note(mtm, "Control one Mac app with your voice. Jev chooses the control; you approve every action. This preview does not run autonomous tasks.", frame(24.,531.,592.,42.));
    ui::allow_wrapping(&intro, 592.);
    view.addSubview(&intro);
    let target = ui::note(
        mtm,
        "Target: open this window from the menu bar while another app is active.",
        frame(24., 500., 592., 24.),
    );
    view.addSubview(&target);
    let tip = ui::note(
        mtm,
        "To change target, switch apps and choose Computer use… in OpenFlow’s menu bar.",
        frame(24., 478., 592., 18.),
    );
    view.addSubview(&tip);
    let key = ui::secure_field(mtm, frame(24., 434., 462., 28.), 0);
    key.setPlaceholderString(Some(&NSString::from_str(
        "TypeSafe API key (not an OpenRouter key)",
    )));
    key.setAccessibilityLabel(Some(&NSString::from_str("TypeSafe API key")));
    view.addSubview(&key);
    let save = ui::button(mtm, frame(496., 433., 120., 30.), "Save key", 0);
    view.addSubview(&save);
    let consent = ui::switch_control(mtm, frame(24., 388., 42., 24.), 0);
    consent.setAccessibilityLabel(Some(&NSString::from_str(
        "Allow sending commands and control labels to TypeSafe this session",
    )));
    view.addSubview(&consent);
    let privacy = ui::note(mtm,"Allow Jev this session: send commands and control labels to TypeSafe. Labels may be sensitive. No screenshots or field values. Local-only blocks this.",frame(76.,370.,540.,48.));
    ui::allow_wrapping(&privacy, 540.);
    view.addSubview(&privacy);
    let voice = ui::note(mtm,"Voice uses your configured speech provider. Commands bypass cleanup, plugins and history. Requires macOS Accessibility permission; voice also requires Microphone.",frame(24.,320.,592.,42.));
    ui::allow_wrapping(&voice, 592.);
    view.addSubview(&voice);
    let command = ui::text_field(mtm, frame(24., 277., 592., 30.), 0);
    command.setAccessibilityLabel(Some(&NSString::from_str("Computer command")));
    command.setPlaceholderString(Some(&NSString::from_str(
        "Focus Search  ·  type \"hello world\" into Search",
    )));
    view.addSubview(&command);
    let record = ui::button(mtm, frame(24., 231., 190., 32.), "Record command", 0);
    view.addSubview(&record);
    let review = ui::button(mtm, frame(228., 231., 190., 32.), "Review command", 0);
    view.addSubview(&review);
    let cancel = ui::button(mtm, frame(432., 231., 184., 32.), "Cancel", 0);
    view.addSubview(&cancel);
    let (scroll, status) = ui::text_view(mtm, frame(24., 74., 592., 143.));
    status.setEditable(false);
    status.setVerticallyResizable(true);
    status.setHorizontallyResizable(false);
    status.setMinSize(NSSize::new(0., 143.));
    status.setMaxSize(NSSize::new(592., f64::MAX));
    // This is our freshly created TextKit 1 view, used only on the main thread.
    if let Some(container) = unsafe { status.textContainer() } {
        container.setWidthTracksTextView(true);
        container.setContainerSize(NSSize::new(592., f64::MAX));
    }
    status.setAccessibilityLabel(Some(&NSString::from_str(
        "Command status and action review",
    )));
    view.addSubview(&scroll);
    let confirm = ui::button(mtm, frame(416., 24., 200., 34.), "Confirm action", 0);
    confirm.setEnabled(false);
    view.addSubview(&confirm);
    let expiry = ui::note(
        mtm,
        "One action. Expires after 30 seconds.",
        frame(24., 30., 370., 22.),
    );
    view.addSubview(&expiry);
    Controls {
        view,
        key,
        save,
        consent,
        command,
        target,
        status,
        record,
        review,
        cancel,
        confirm,
    }
}

pub fn preview_views(mtm: MainThreadMarker) -> Vec<(String, Retained<NSView>)> {
    let ready = controls(mtm);
    ready
        .target
        .setStringValue(&NSString::from_str("Target: Example Notes"));
    ready.status.setString(&NSString::from_str("Add your TypeSafe key, enable session sharing, then type or record one command. Voice stops automatically after 15 seconds."));
    let review = controls(mtm);
    review
        .target
        .setStringValue(&NSString::from_str("Target: Example Notes"));
    review
        .command
        .setStringValue(&NSString::from_str("type \"hello world\" into Search"));
    review.consent.setState(NSControlStateValueOn);
    review.confirm.setEnabled(true);
    review
        .status
        .setString(&NSString::from_str(&describe_proposal(
            "Example Notes",
            "Draft",
            &computer_use::Element {
                id: "e0".into(),
                role: "AXTextField".into(),
                label: "Search".into(),
                actions: vec!["type".into()],
            },
            &Proposal {
                action: "type".into(),
                target: "e0".into(),
                text: Some("hello world".into()),
            },
        )));
    vec![
        ("computer-use-setup".into(), ready.view),
        ("computer-use-review".into(), review.view),
    ]
}

impl ComputerUseWindow {
    pub fn is_visible(&self) -> bool {
        self.ivars().window.isVisible()
    }
    pub fn present(&self) {
        if let Some(app) = NSWorkspace::sharedWorkspace().frontmostApplication() {
            let pid = app.processIdentifier();
            if pid != std::process::id() as i32 {
                self.reset();
                self.ivars().pid.set(pid);
                let name = app
                    .localizedName()
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "Mac app".into());
                self.ivars()
                    .target
                    .setStringValue(&NSString::from_str(&format!("Target: {name}")));
                *self.ivars().application.borrow_mut() = name;
            }
        }
        ui::present_window(&self.ivars().window, "computer-use");
    }
    fn message(&self, text: &str) {
        self.ivars().status.setString(&NSString::from_str(text));
    }
    fn busy(&self, busy: bool) {
        self.ivars().review.setEnabled(!busy);
        self.ivars().command.setEnabled(!busy);
        self.ivars().record.setEnabled(!busy);
    }
    fn reset(&self) {
        self.ivars().session.borrow_mut().invalidate();
        if let Some(stop) = self.ivars().stop.borrow_mut().take() {
            stop.cancel();
        }
        self.ivars().snapshot.borrow_mut().take();
        self.ivars().confirm.setEnabled(false);
        self.ivars()
            .record
            .setTitle(&NSString::from_str("Record command"));
        self.busy(false);
    }
    fn allowed(&self) -> bool {
        if self.ivars().engine.settings().local_only() {
            self.message("Local-only protection is enabled. Jev needs a cloud connection; this feature will not send anything.");
            false
        } else if self.ivars().consent.state() != NSControlStateValueOn {
            self.message("Enable session sharing first. Jev receives your command and the target window’s control labels.");
            false
        } else if self.ivars().pid.get() <= 0 {
            self.message("Switch to the app you want to control, then open Computer use from OpenFlow’s menu bar.");
            false
        } else {
            true
        }
    }
    fn review(&self) {
        self.reset();
        if !self.allowed() {
            return;
        }
        let command = self.ivars().command.stringValue().to_string();
        if command.trim().is_empty() || command.len() > computer_use::MAX_COMMAND {
            self.message("Enter one short command (up to 2,000 bytes).");
            return;
        }
        let snapshot = match computer_access::snapshot(
            self.ivars().pid.get(),
            &self.ivars().application.borrow(),
        ) {
            Ok(s) => s,
            Err(e) => {
                self.message(&e);
                return;
            }
        };
        let screen = snapshot.screen.clone();
        *self.ivars().snapshot.borrow_mut() = Some(snapshot);
        let generation = self.ivars().session.borrow().generation();
        let cancel = self.ivars().session.borrow().cancellation();
        let engine = Arc::clone(&self.ivars().engine);
        self.busy(true);
        self.message("Jev is choosing an action. Nothing will execute until you confirm.");
        crate::app::spawn(async move {
            let credentials = tokio::task::spawn_blocking(move || {
                (
                    engine.settings().get("jev_api_key"),
                    engine.settings().local_only(),
                )
            })
            .await;
            let result = match credentials {
                Ok((Ok(key), local_only)) => {
                    let key = key.unwrap_or_default();
                    tokio::select! { biased;
                        _=cancel.cancelled()=>return,
                        result=computer_use::decide(&command,&screen,&key,local_only)=>result,
                    }
                }
                _ => Err("Could not read the TypeSafe key from Keychain.".into()),
            };
            deliver(generation, move |window| {
                window.busy(false);
                let snapshot = window.ivars().snapshot.borrow_mut().take();
                if !window.allowed() {
                    return;
                }
                match (result, snapshot) {
                    (Ok(proposal), Some(snapshot)) => {
                        let element = snapshot
                            .screen
                            .elements
                            .iter()
                            .find(|e| e.id == proposal.target)
                            .expect("validated target");
                        window.message(&describe_proposal(
                            &snapshot.screen.application,
                            &snapshot.window_label,
                            element,
                            &proposal,
                        ));
                        window
                            .ivars()
                            .session
                            .borrow_mut()
                            .offer(generation, (snapshot, proposal, command));
                        window.ivars().confirm.setEnabled(true);
                    }
                    (Err(error), _) => window.message(&error),
                    _ => window.message("The target expired. Review a new command."),
                }
            });
        });
    }
    fn record(&self) {
        if let Some(stop) = self.ivars().stop.borrow().as_ref() {
            stop.cancel();
            self.ivars().record.setEnabled(false);
            self.message("Transcribing your command…");
            return;
        }
        self.reset();
        if !self.allowed() {
            return;
        }
        if !computer_access::trusted() {
            self.message("Enable OpenFlow in System Settings → Privacy & Security → Accessibility before recording a computer command.");
            return;
        }
        let generation = self.ivars().session.borrow().generation();
        let cancel = self.ivars().session.borrow().cancellation();
        let stop = CancellationToken::new();
        *self.ivars().stop.borrow_mut() = Some(stop.clone());
        let engine = Arc::clone(&self.ivars().engine);
        self.busy(true);
        self.ivars().record.setEnabled(true);
        self.ivars()
            .record
            .setTitle(&NSString::from_str("Stop & review"));
        self.message("Recording one command (maximum 15 seconds). Cancel discards it.");
        crate::app::spawn(async move {
            let capture_engine = Arc::clone(&engine);
            let startup_cancel = cancel.clone();
            // Always await startup and release its guard, even if cancellation arrived during startup.
            let capture = tokio::task::spawn_blocking(move || {
                    if startup_cancel.is_cancelled() {
                        return Err("Recording cancelled.".into());
                    }
                    if capture_engine.settings().local_only() {
                        return Err("Local-only protection is enabled. Computer command recording was not started.".into());
                    }
                    let key=capture_engine.settings().get("jev_api_key")?;
                    if key.as_deref().is_none_or(|k|k.trim().is_empty()) {
                        return Err("Save a TypeSafe API key before recording a computer command.".into());
                    }
                    if startup_cancel.is_cancelled() {return Err("Recording cancelled.".into());}
                    capture_engine.begin_command_capture()
            })
            .await;
            let result = match capture {
                Ok(Ok(capture)) => {
                    tokio::select! { biased; _=cancel.cancelled()=>{}, _=stop.cancelled()=>{}, _=tokio::time::sleep(Duration::from_secs(15))=>{} }
                    let wav = tokio::task::spawn_blocking(move || capture.finish()).await;
                    if cancel.is_cancelled() {
                        return;
                    }
                    deliver(generation, |w| {
                        w.ivars().record.setEnabled(false);
                        w.message("Transcribing your command…");
                    });
                    match wav {
                        Ok(Ok(wav)) => tokio::select! { biased;
                            _=cancel.cancelled()=>return,
                            result=tokio::time::timeout(Duration::from_secs(45),engine.transcribe_wav(wav))=>match result {
                                Ok(Ok((text,_)))=>Ok(text),Ok(Err(e))=>Err(e),Err(_)=>Err("Command transcription timed out.".into()),
                            }
                        },
                        _ => Err("Could not finish recording. Please try again.".into()),
                    }
                }
                Ok(Err(e)) => Err(e),
                Err(_) => Err("Could not start command recording.".into()),
            };
            deliver(generation, move |window| {
                window.reset();
                match result {
                    Ok(text) => {
                        window
                            .ivars()
                            .command
                            .setStringValue(&NSString::from_str(&text));
                        window.review();
                    }
                    Err(error) => window.message(&error),
                }
            });
        });
    }
}

fn deliver(generation: u64, body: impl FnOnce(&ComputerUseWindow) + Send + 'static) {
    crate::events::on_main(move || {
        crate::app::with_app(|app| {
            app.with_computer_use(|window| {
                let accepts = window.ivars().session.borrow().accepts(generation);
                if accepts {
                    body(window);
                }
            });
        });
    });
}

fn describe_proposal(
    application: &str,
    title: &str,
    element: &computer_use::Element,
    proposal: &Proposal,
) -> String {
    // Debug formatting makes invisible/bidi characters explicit in untrusted
    // labels and payloads; no truncation of the text that will actually be typed.
    format!("Review before confirming\nApp: {application:?}\nWindow: {title:?}\nAction: {}\nControl: {:?} ({}, {})\n{}\nThis can change data in the target app. Check the target and payload carefully.",proposal.action,element.label,element.role,element.id,
        proposal.text.as_ref().map(|t|format!("Exact text (replaces selected text; escaped display):\n{t:?}\n")).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn review_keeps_the_entire_payload_and_exposes_invisible_characters() {
        let element = computer_use::Element {
            id: "e0".into(),
            role: "AXTextField".into(),
            label: "Search\u{202e}".into(),
            actions: vec!["type".into()],
        };
        let text = "x".repeat(1900);
        let proposal = Proposal {
            action: "type".into(),
            target: "e0".into(),
            text: Some(text.clone()),
        };
        let review = describe_proposal("Fixture", "Draft", &element, &proposal);
        assert!(review.contains(&text));
        assert!(review.contains("\\u{202e}"));
        assert!(!review.contains('\u{202e}'));
        assert!(review.contains("Draft"));
        assert!(review.contains("replaces selected text"));
    }
}
