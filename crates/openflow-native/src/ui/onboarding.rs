//!
//! Presented as a sheet on the main window rather than as a window of its own.
//! It is still an `NSWindow` -- a sheet is -- but it hangs off the one window
//! the app now has, so setup happens over the workspace it is setting up
//! instead of beside it. `Closable` is kept even though a sheet draws no close
//! button: `performClose:` still runs, so the Cmd+W in the app menu is still
//! the way out of a wizard the user does not want to finish.
//! First-run setup: the native form of `App.tsx`'s onboarding screen.
//!
//! The web wizard has three steps (provider, credentials, models). This one has
//! five, because two things the web build did elsewhere have nowhere else to go
//! in a menu bar app: a welcome panel that says what the hotkey does, and a
//! closing panel that says setup is saved and hands the user to Settings. The
//! copy, the provider list, the recommended badge, the "an empty key is only
//! valid for a custom endpoint" rule and the model defaults are the web
//! screen's, verbatim where they fit an AppKit control.
//!
//! Nothing is written until the user finishes: the web wizard also saves once,
//! in `finishOnboarding`, and a wizard that wrote as it went would leave a
//! half-configured provider behind if it were closed halfway. The one exception
//! is the record shortcut, which is registered with the system the moment it is
//! recorded, exactly as the Settings window does it.

use std::cell::{Cell, RefCell};
use std::sync::Arc;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{
    define_class, msg_send, sel, AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly,
    Message,
};
use objc2_app_kit::{
    NSBackingStoreType, NSButton, NSComboBox, NSControl, NSControlStateValueOff,
    NSControlStateValueOn, NSControlTextEditingDelegate, NSPopUpButton, NSSecureTextField,
    NSSwitch, NSTabView, NSTabViewItem, NSTabViewType, NSTextField, NSView, NSWindow,
    NSWindowDelegate, NSWindowStyleMask,
};
use objc2_foundation::{
    NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString,
};

use openflow_core::engine::Engine;
use openflow_core::transcribe::ModelInfo;

use crate::ui::recorder::ChordRecorder;
use crate::ui::settings::{join_provider, split_provider};
use crate::ui::{
    button, combo, label, note, popup, radio, secure_field, switch_control, text_field, wire, wrap,
    Form, ROW,
};

const WINDOW_WIDTH: f64 = 520.0;
/// The panel area, and the window that has to hold the tallest panel.
///
/// `Form` lays out downward from the height it was given and does not stop at
/// zero, and an `NSView` does not clip its subviews, so a panel that outgrows
/// [`STEP_HEIGHT`] is not cropped -- it keeps going past the bottom of the tab
/// view and draws on top of the error line and the Back/Continue row. That is
/// what the provider panel was doing: measured at build time the five panels
/// come to 170, **461**, 257, 205 and 150pt, and the provider one was being
/// laid out into 380.
///
/// So this is the tallest panel, not a guess, and the window is that plus the
/// 140pt of chrome around it (68 above for the kicker and heading, 72 below for
/// the error line and the buttons). A panel that grows past it goes back to
/// drawing over the buttons, so re-measure when one does: the number is the
/// lowest subview origin in each panel view, subtracted from `STEP_HEIGHT`.
const WINDOW_HEIGHT: f64 = STEP_HEIGHT + 140.0;
const STEP_WIDTH: f64 = 488.0;
const STEP_HEIGHT: f64 = 461.0;

// ── The step machine ──────────────────────────────────────

/// One panel of the wizard.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Step {
    Welcome,
    Provider,
    Credentials,
    Preferences,
    Done,
}

impl Step {
    pub const ORDER: [Step; 5] = [
        Step::Welcome,
        Step::Provider,
        Step::Credentials,
        Step::Preferences,
        Step::Done,
    ];

    pub fn index(self) -> usize {
        Self::ORDER
            .iter()
            .position(|step| *step == self)
            .unwrap_or(0)
    }

    /// The next panel, or this one when there is nowhere further to go. The
    /// caller decides whether it is allowed to move; see [`can_advance`].
    pub fn next(self) -> Step {
        let index = (self.index() + 1).min(Self::ORDER.len() - 1);
        Self::ORDER[index]
    }

    pub fn back(self) -> Step {
        Self::ORDER[self.index().saturating_sub(1)]
    }

    /// The heading, matching the web wizard where the panel matches.
    pub fn title(self) -> &'static str {
        match self {
            Step::Welcome => "Say it once. Keep moving.",
            Step::Provider => "Choose how OpenFlow listens",
            Step::Credentials => "Connect your provider",
            Step::Preferences => "Make it yours",
            Step::Done => "OpenFlow is ready",
        }
    }

    /// The web wizard's step kicker, counted over this wizard's panels.
    pub fn kicker(self) -> String {
        format!("Setup · {} of {}", self.index() + 1, Self::ORDER.len())
    }

    /// What the primary button says on this panel.
    pub fn primary_title(self) -> &'static str {
        match self {
            Step::Welcome => "Get started",
            Step::Provider => "Continue to connection",
            Step::Credentials => "Continue to preferences",
            Step::Preferences => "Finish setup",
            Step::Done => "Open settings",
        }
    }
}

/// `App.tsx`'s `validateProviderConfiguration`, same rule and same words: a key
/// may only be empty for a custom endpoint, and a custom endpoint needs a whole
/// URL.
pub fn validate_provider(kind: &str, url: &str, key: &str) -> Result<(), String> {
    // The on-this-Mac card has no credential to validate: that is the point of
    // it. Setup for it finishes on the provider panel.
    if is_local_card(kind) {
        return Ok(());
    }
    if key.trim().is_empty() && kind != "custom" {
        return Err("Enter an API key to continue.".to_string());
    }
    if kind == "custom" {
        let url = url.trim().to_ascii_lowercase();
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Err(
                "Enter a complete endpoint URL beginning with http:// or https://.".to_string(),
            );
        }
    }
    Ok(())
}

/// Whether the primary button may leave `step`, and why not when it may not.
///
/// The connection gate is the web wizard's: its Continue button only appears
/// once `verifyConnection` has succeeded, so a key that cannot list models
/// never reaches the rest of setup.
pub fn can_advance(
    step: Step,
    kind: &str,
    url: &str,
    key: &str,
    connected: bool,
) -> Result<(), String> {
    if is_local_card(kind) {
        return Ok(());
    }
    match step {
        Step::Credentials => {
            validate_provider(kind, url, key)?;
            if !connected {
                return Err("Test the connection before continuing.".to_string());
            }
            Ok(())
        }
        Step::Preferences => validate_provider(kind, url, key),
        _ => Ok(()),
    }
}

/// Whether the credentials on screen have been proven against the provider.
///
/// A plain bool was not enough. The wizard set it on a successful Test
/// connection and cleared it only when the provider changed, so editing the key
/// or the endpoint afterwards left the gate open and setup could save a key
/// nothing had ever called. `App.tsx` resets `connectionState` on every
/// keystroke in either credential field (src/App.tsx:962 and :979); this is
/// that rule as one value with two verbs, so the window cannot forget half of
/// it.
#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
pub struct Connection {
    proven: bool,
}

impl Connection {
    pub fn proven(self) -> bool {
        self.proven
    }

    /// `fetch_models` answered for these exact credentials.
    pub fn succeeded(&mut self) {
        self.proven = true;
    }

    /// The credentials changed, or the call failed. Either way what was proven
    /// no longer describes what is on screen.
    pub fn invalidated(&mut self) {
        self.proven = false;
    }
}

/// One row of the provider list.
pub struct ProviderOption {
    pub value: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub recommended: bool,
    /// The provider's shipped speech-to-text and cleanup models, which fill the
    /// model fields when the user has not chosen their own.
    pub stt_default: &'static str,
    pub chat_default: &'static str,
}

/// The web wizard's provider grid, in its order: Groq first and recommended.
pub const PROVIDER_OPTIONS: &[ProviderOption] = &[
    ProviderOption {
        value: "groq",
        label: "Groq",
        description: "Fastest Whisper and cleanup. One key, no proxy hop.",
        recommended: true,
        stt_default: "whisper-large-v3-turbo",
        chat_default: "openai/gpt-oss-20b",
    },
    ProviderOption {
        value: "openrouter",
        label: "OpenRouter",
        description: "One key for many models, plus Gemini voice.",
        recommended: false,
        stt_default: "openai/whisper-1",
        chat_default: "google/gemini-3.1-flash-lite-preview",
    },
    ProviderOption {
        value: "openai",
        label: "OpenAI",
        description: "Reliable speech-to-text and compact formatting models.",
        recommended: false,
        stt_default: "whisper-1",
        chat_default: "gpt-4o-mini",
    },
    ProviderOption {
        value: "deepgram",
        label: "Deepgram",
        description: "Nova speech recognition with broad language coverage.",
        recommended: false,
        stt_default: "nova-3",
        chat_default: "openai/gpt-oss-20b",
    },
    ProviderOption {
        value: "custom",
        label: "Self-hosted / LAN",
        description: "Connect any OpenAI-compatible speech or chat service.",
        recommended: false,
        stt_default: "whisper-large-v3",
        chat_default: "default",
    },
];

/// The stored value the on-this-Mac card stands for. Not a member of
/// [`PROVIDER_OPTIONS`]: it is a *backend*, not a provider -- it has no key, no
/// endpoint and no model list to test -- so it sits beside the grid as its own
/// radio and short-circuits the rest of the wizard.
pub const LOCAL_CARD: &str = "local";
/// The card's title and the sentence under it.
pub const LOCAL_CARD_LABEL: &str = "On this Mac (private)";
pub const LOCAL_CARD_DESCRIPTION: &str =
    "Runs Qwen3-ASR on this Mac. No key, no network, and no audio leaves the machine. Needs Python and a one-time download.";

/// Whether a wizard selection is the on-this-Mac card rather than a provider.
pub fn is_local_card(kind: &str) -> bool {
    kind == LOCAL_CARD
}

pub fn option_for(kind: &str) -> &'static ProviderOption {
    PROVIDER_OPTIONS
        .iter()
        .find(|option| option.value == kind)
        .unwrap_or(&PROVIDER_OPTIONS[0])
}

/// The badge the web grid paints on a recommended provider.
pub fn badge_text(option: &ProviderOption) -> Option<&'static str> {
    option.recommended.then_some("Recommended")
}

/// The closing panel's summary of what setup saved.
///
/// The shortcut is named as already live, because it is: the recorder rebinds
/// it with the system the moment it is recorded, so by the time this panel is
/// on screen the chord already works everywhere.
pub fn summary_line(kind: &str, model: &str, microphone: &str, shortcut: &str) -> String {
    let provider = option_for(kind).label;
    let model = if model.trim().is_empty() {
        option_for(kind).stt_default
    } else {
        model.trim()
    };
    format!(
        "{} · {} · {} · hold {} to dictate, active now",
        provider, model, microphone, shortcut
    )
}

// ── Control tags ──────────────────────────────────────────

const TAG_PROVIDER_BASE: isize = 100;
const TAG_LOCAL_CARD: isize = 99;
const TAG_SAME_PROVIDER: isize = 10;
const TAG_FORMATTING_PROVIDER: isize = 11;

struct Controls {
    kicker: Retained<NSTextField>,
    heading: Retained<NSTextField>,
    error: Retained<NSTextField>,
    back: Retained<NSButton>,
    /// The way out of a wizard the user does not want to finish.
    later: Retained<NSButton>,
    primary: Retained<NSButton>,

    providers: Vec<Retained<NSButton>>,
    /// The on-this-Mac card. In the same radio group as `providers` (AppKit
    /// groups by superview and action) but not in that list, because it stands
    /// for a backend rather than an entry in `PROVIDER_OPTIONS`.
    local_card: Retained<NSButton>,
    same_provider: Retained<NSSwitch>,
    formatting_provider: Retained<NSPopUpButton>,

    provider_url: Retained<NSTextField>,
    api_key: Retained<NSSecureTextField>,
    connection_status: Retained<NSTextField>,
    test: Retained<NSButton>,

    stt_model: Retained<NSComboBox>,
    chat_model: Retained<NSComboBox>,
    microphone: Retained<NSPopUpButton>,
    microphone_ids: RefCell<Vec<String>>,
    refresh: Retained<NSButton>,
    hotkey: Retained<NSButton>,

    summary: Retained<NSTextField>,
}

pub struct OnboardingIvars {
    engine: Arc<Engine>,
    window: Retained<NSWindow>,
    panels: Retained<NSTabView>,
    controls: Controls,
    step: Cell<Step>,
    connection: Cell<Connection>,
    recorder: ChordRecorder,
    recording: Cell<bool>,
}

define_class!(
    // SAFETY: NSObject imposes no subclassing requirements; this class holds
    // only ivars and implements no Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "OpenFlowOnboardingWindow"]
    #[ivars = OnboardingIvars]
    pub struct OnboardingWindow;

    unsafe impl NSObjectProtocol for OnboardingWindow {}

    unsafe impl NSWindowDelegate for OnboardingWindow {
        /// Hide, never close: setup can be reopened from Settings and the
        /// window is built once.
        /// Cmd+W, since a sheet draws no close button. Hiding, not closing:
        /// the wizard is built once and kept.
        #[unsafe(method(windowShouldClose:))]
        fn window_should_close(&self, _sender: &NSWindow) -> bool {
            self.stop_recording_hotkey();
            self.ivars().window.makeFirstResponder(None);
            crate::ui::dismiss_sheet(&self.ivars().window, "onboarding");
            false
        }
    }

    unsafe impl NSControlTextEditingDelegate for OnboardingWindow {
        /// The endpoint URL and the API key are the two fields this object is
        /// the delegate of, and editing either one un-proves the connection.
        /// Without this a user could pass the Test connection gate and then
        /// paste a different key over it on the way out.
        #[unsafe(method(controlTextDidChange:))]
        fn control_text_did_change(&self, _notification: &NSNotification) {
            self.invalidate_connection();
        }
    }

    impl OnboardingWindow {
        #[unsafe(method(stepBack:))]
        fn step_back(&self, _sender: &NSControl) {
            let step = self.ivars().step.get();
            self.show_step(step.back());
        }

        #[unsafe(method(stepNext:))]
        fn step_next(&self, _sender: &NSControl) {
            self.advance();
        }

        /// Leave setup without finishing it. Nothing is written -- the wizard
        /// only writes on Finish -- so the app is exactly as configured, or
        /// unconfigured, as it was before this was opened.
        #[unsafe(method(skipSetup:))]
        fn skip_setup(&self, _sender: &NSControl) {
            self.ivars().window.makeFirstResponder(None);
            crate::ui::dismiss_sheet(&self.ivars().window, "onboarding");
        }

        #[unsafe(method(providerChanged:))]
        fn provider_changed(&self, _sender: &NSControl) {
            // A different provider is a different key and a different model
            // list, so the connection has to be proven again.
            self.invalidate_connection();
            self.apply_provider_defaults();
            self.update_chrome();
        }

        #[unsafe(method(testConnection:))]
        fn test_connection(&self, _sender: &NSControl) {
            self.request_models();
        }

        #[unsafe(method(refreshMicrophones:))]
        fn refresh_microphones(&self, _sender: &NSControl) {
            self.reload_microphones();
        }

        #[unsafe(method(recordHotkey:))]
        fn record_hotkey(&self, _sender: &NSControl) {
            self.start_recording_hotkey();
        }
    }
);

impl OnboardingWindow {
    pub fn new(app: &std::rc::Rc<crate::app::App>, mtm: MainThreadMarker) -> Retained<Self> {
        let engine = Arc::clone(app.engine());

        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                NSRect::new(
                    NSPoint::new(0.0, 0.0),
                    NSSize::new(WINDOW_WIDTH, WINDOW_HEIGHT),
                ),
                // No Resizable: the panels are laid out at a fixed size, so a
                // resize would only ever crop them.
                NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        window.setTitle(&NSString::from_str("Set up OpenFlow"));
        unsafe { window.setReleasedWhenClosed(false) };
        window.center();

        let (panels, controls) = build_panels(mtm);
        if let Some(content) = window.contentView() {
            content.addSubview(&controls.kicker);
            content.addSubview(&controls.heading);
            content.addSubview(&panels);
            content.addSubview(&controls.error);
            content.addSubview(&controls.back);
            content.addSubview(&controls.later);
            content.addSubview(&controls.primary);
        }

        let this = Self::alloc(mtm).set_ivars(OnboardingIvars {
            engine,
            window,
            panels,
            controls,
            step: Cell::new(Step::Welcome),
            connection: Cell::new(Connection::default()),
            recorder: ChordRecorder::default(),
            recording: Cell::new(false),
        });
        let this: Retained<Self> = unsafe { msg_send![super(this), init] };

        this.ivars()
            .window
            .setDelegate(Some(ProtocolObject::from_ref(&*this)));
        this.wire_actions();
        this.reload();
        this
    }

    fn wire_actions(&self) {
        let controls = &self.ivars().controls;
        let target: &AnyObject = self.as_ref();
        for option in &controls.providers {
            wire(option, target, sel!(providerChanged:));
        }
        // Same action as the provider radios, which is also what puts it in
        // their radio group.
        wire(&controls.local_card, target, sel!(providerChanged:));
        wire(&controls.same_provider, target, sel!(providerChanged:));
        wire(
            &controls.formatting_provider,
            target,
            sel!(providerChanged:),
        );
        wire(&controls.back, target, sel!(stepBack:));
        wire(&controls.later, target, sel!(skipSetup:));
        wire(&controls.primary, target, sel!(stepNext:));
        wire(&controls.test, target, sel!(testConnection:));
        // The two credential fields report every keystroke, so the connection
        // gate can close again the moment one of them changes.
        for field in [
            controls.provider_url.as_ref() as &NSControl,
            &controls.api_key,
        ] {
            unsafe {
                msg_send![
                    field,
                    setDelegate: Some(ProtocolObject::<dyn NSControlTextEditingDelegate>::from_ref(self))
                ]
            }
        }
        wire(&controls.refresh, target, sel!(refreshMicrophones:));
        wire(&controls.hotkey, target, sel!(recordHotkey:));
    }

    /// On screen, as the Dock-icon rule reads it.
    pub fn is_visible(&self) -> bool {
        self.ivars().window.isVisible()
    }

    /// Hang the wizard off `parent` as a sheet.
    pub fn present_on(&self, parent: &NSWindow) {
        crate::ui::present_sheet(parent, &self.ivars().window, "onboarding");
    }

    /// Fill the controls from whatever is already saved and start at the front.
    /// Reopening from Settings shows the current configuration, not a blank
    /// form.
    pub fn reload(&self) {
        let ivars = self.ivars();
        let settings = ivars.engine.settings();
        let controls = &ivars.controls;

        let (kind, url) = split_provider(&settings.provider_name());
        self.select_provider(if settings.is_local_backend() {
            LOCAL_CARD
        } else {
            &kind
        });
        self.set_text(&controls.provider_url, &url);
        self.set_text(
            &controls.api_key,
            &settings.api_key().ok().flatten().unwrap_or_default(),
        );
        set_switch(&controls.same_provider, settings.same_provider());
        let (formatting, _) = split_provider(
            &settings
                .formatting_provider_name()
                .unwrap_or_else(|| settings.provider_name()),
        );
        select_provider_popup(&controls.formatting_provider, &formatting);
        controls.stt_model.setStringValue(&NSString::from_str(
            &settings.stt_model().unwrap_or_default(),
        ));
        controls.chat_model.setStringValue(&NSString::from_str(
            &settings.chat_model().unwrap_or_default(),
        ));
        self.reload_microphones();
        self.reload_hotkey();
        self.set_text(&controls.connection_status, "");
        self.set_text(&controls.error, "");
        ivars.connection.set(Connection::default());
        self.apply_provider_defaults();
        self.show_step(Step::Welcome);
    }

    fn reload_hotkey(&self) {
        let ivars = self.ivars();
        let text = ivars
            .engine
            .settings()
            .shortcut("record")
            .map(|shortcut| crate::hotkeys::describe(&shortcut))
            .unwrap_or_else(|_| "Not set".to_string());
        ivars.controls.hotkey.setTitle(&NSString::from_str(&text));
    }

    fn reload_microphones(&self) {
        let ivars = self.ivars();
        let controls = &ivars.controls;
        let devices = ivars.engine.list_audio_devices().unwrap_or_default();
        let mut ids = vec![String::new()];
        controls.microphone.removeAllItems();
        controls
            .microphone
            .addItemWithTitle(&NSString::from_str("System default"));
        for device in &devices {
            let title = if device.is_default {
                format!("{} (default)", device.name)
            } else {
                device.name.clone()
            };
            controls
                .microphone
                .addItemWithTitle(&NSString::from_str(&title));
            ids.push(device.id.clone());
        }
        let saved = ivars.engine.settings().microphone().unwrap_or_default();
        let index = ids.iter().position(|id| *id == saved).unwrap_or(0);
        controls.microphone.selectItemAtIndex(index as isize);
        *controls.microphone_ids.borrow_mut() = ids;
    }

    // ── Step machine ──────────────────────────────────────

    fn show_step(&self, step: Step) {
        let ivars = self.ivars();
        self.stop_recording_hotkey();
        ivars.step.set(step);
        ivars.panels.selectTabViewItemAtIndex(step.index() as isize);
        if step == Step::Done {
            self.fill_summary();
        }
        self.set_text(&ivars.controls.error, "");
        self.update_chrome();
    }

    /// Titles, enablement and the header, for whichever panel is showing.
    fn update_chrome(&self) {
        let ivars = self.ivars();
        let controls = &ivars.controls;
        let step = ivars.step.get();
        self.set_text(&controls.kicker, &step.kicker());
        self.set_text(&controls.heading, step.title());
        controls
            .primary
            .setTitle(&NSString::from_str(step.primary_title()));
        controls.back.setHidden(step == Step::Welcome);

        // Deepgram transcribes only, so one provider cannot serve both. The web
        // wizard disables the toggle for it (src/App.tsx:934); here it is
        // switched off as well, or "same" would mean "clean up with a provider
        // that cannot".
        let kind = self.selected_provider();
        if step == Step::Provider && is_local_card(&kind) {
            controls
                .primary
                .setTitle(&NSString::from_str("Set up on this Mac"));
        }
        let deepgram = kind == "deepgram";
        if deepgram && is_on(&controls.same_provider) {
            set_switch(&controls.same_provider, false);
        }
        controls.same_provider.setEnabled(!deepgram);

        // The web wizard hides the cleanup provider behind the same-provider
        // toggle; here it is present and inert, so the panel does not reflow.
        // While it is inert it shows the provider that will actually be used,
        // which is the transcription provider, because that is what `save`
        // stores.
        let same = is_on(&controls.same_provider);
        if same {
            select_provider_popup(&controls.formatting_provider, &kind);
        }
        controls.formatting_provider.setEnabled(!same);
    }

    fn advance(&self) {
        let ivars = self.ivars();
        let step = ivars.step.get();
        if step == Step::Done {
            self.finish();
            return;
        }
        // The on-this-Mac card has nothing left to ask: no key to test, no
        // endpoint to type, no model list to fetch. Save the choice and hand
        // the user to the panel where the install and the download live.
        if step == Step::Provider && is_local_card(&self.selected_provider()) {
            if let Err(error) = self.save_local() {
                self.set_text(&ivars.controls.error, &error);
                return;
            }
            // Same exit as `finish`: the wizard is a sheet on the main window
            // now, and the runner's Install and Download live on the Settings
            // page, so that is where the local card lands the user.
            crate::ui::dismiss_sheet(&self.ivars().window, "onboarding");
            crate::app::with_app(|app| {
                app.with_settings(|page| page.reload());
                app.show_main(Some("settings"));
            });
            return;
        }
        let (kind, url, key) = self.provider_fields();
        let proven = ivars.connection.get().proven();
        if let Err(error) = can_advance(step, &kind, &url, &key, proven) {
            self.set_text(&ivars.controls.error, &error);
            return;
        }
        if step == Step::Preferences {
            if let Err(error) = self.save() {
                self.set_text(&ivars.controls.error, &error);
                return;
            }
        }
        self.show_step(step.next());
    }

    /// The provider kind, its endpoint URL and its key as the controls hold
    /// them right now.
    fn provider_fields(&self) -> (String, String, String) {
        let controls = &self.ivars().controls;
        let kind = self.selected_provider();
        let url = string_value(&controls.provider_url);
        let key = string_value(&controls.api_key);
        (kind, url, key)
    }

    fn selected_provider(&self) -> String {
        let controls = &self.ivars().controls;
        if controls.local_card.state() == NSControlStateValueOn {
            return LOCAL_CARD.to_string();
        }
        for (index, option) in controls.providers.iter().enumerate() {
            if option.state() == NSControlStateValueOn {
                return PROVIDER_OPTIONS[index].value.to_string();
            }
        }
        PROVIDER_OPTIONS[0].value.to_string()
    }

    fn select_provider(&self, kind: &str) {
        let controls = &self.ivars().controls;
        let local = is_local_card(kind);
        controls.local_card.setState(if local {
            NSControlStateValueOn
        } else {
            NSControlStateValueOff
        });
        let selected = PROVIDER_OPTIONS
            .iter()
            .position(|option| option.value == kind)
            .unwrap_or(0);
        for (index, button) in controls.providers.iter().enumerate() {
            button.setState(if index == selected && !local {
                NSControlStateValueOn
            } else {
                NSControlStateValueOff
            });
        }
    }

    /// Put the provider's shipped model ids in the model fields, but only where
    /// the user has not typed one. Same rule as the web wizard's placeholders.
    fn apply_provider_defaults(&self) {
        let controls = &self.ivars().controls;
        let option = option_for(&self.selected_provider());
        if string_value(&controls.stt_model).trim().is_empty() {
            controls
                .stt_model
                .setStringValue(&NSString::from_str(option.stt_default));
        }
        if string_value(&controls.chat_model).trim().is_empty() {
            controls
                .chat_model
                .setStringValue(&NSString::from_str(option.chat_default));
        }
    }

    fn fill_summary(&self) {
        let ivars = self.ivars();
        let controls = &ivars.controls;
        let microphone = controls
            .microphone
            .titleOfSelectedItem()
            .map(|title| title.to_string())
            .unwrap_or_else(|| "System default".to_string());
        let shortcut = controls.hotkey.title().to_string();
        let line = summary_line(
            &self.selected_provider(),
            &string_value(&controls.stt_model),
            &microphone,
            &shortcut,
        );
        self.set_text(&controls.summary, &line);
    }

    // ── Saving ────────────────────────────────────────────

    /// Write every key the wizard collected, in the formats `ui::settings`
    /// writes them, so the two windows agree about what is stored.
    fn save(&self) -> Result<(), String> {
        let ivars = self.ivars();
        let settings = ivars.engine.settings();
        let controls = &ivars.controls;
        let (kind, url, key) = self.provider_fields();
        validate_provider(&kind, &url, &key)?;

        // Finishing the wizard on a provider is also how a user leaves the
        // local backend; without this the rows below would be saved and
        // ignored, because the engine would still be transcribing on-device.
        settings.set("transcription_backend", "remote")?;
        settings.set("provider", &join_provider(&kind, &url))?;
        settings.set("api_key", key.trim())?;
        settings.set(
            "same_provider",
            bool_setting(is_on(&controls.same_provider)),
        )?;
        // Written every time, never conditionally, and the earlier note here
        // was wrong about why. The pipeline does not read this row while "same
        // for cleanup" is on: `run_pipeline` takes the transcription provider
        // in that branch and only reaches `formatting_provider_name` in the
        // else (crates/openflow-core/src/engine.rs:438-445). Nothing was
        // unsafe. It is written anyway so the row describes the provider the
        // engine would actually use, which is what Settings reads back and what
        // a later flip of "same" starts from; a stale row means the Settings
        // window shows a cleanup provider that is not the one cleaning up.
        //
        // One divergence from the web build, deliberate: App.tsx:654-660 leaves
        // this row alone while "same" is on, so turning "same" off later
        // restores the cleanup provider chosen before. Here the row carries the
        // transcription provider, which is what the disabled popup shows, so
        // that earlier choice is not preserved.
        let index = controls.formatting_provider.indexOfSelectedItem().max(0) as usize;
        let formatting = if is_on(&controls.same_provider) {
            kind.as_str()
        } else {
            formatting_options()
                .get(index)
                .copied()
                .unwrap_or(PROVIDER_OPTIONS[0].value)
        };
        // A custom cleanup endpoint reuses the transcription URL: the wizard
        // asks for one endpoint, and Settings is where a second one is
        // configured.
        settings.set("formatting_provider", &join_provider(formatting, &url))?;
        settings.set("stt_model", string_value(&controls.stt_model).trim())?;
        settings.set("chat_model", string_value(&controls.chat_model).trim())?;
        let index = controls.microphone.indexOfSelectedItem().max(0) as usize;
        let ids = controls.microphone_ids.borrow();
        settings.set(
            "microphone",
            ids.get(index).map(String::as_str).unwrap_or(""),
        )?;
        Ok(())
    }

    /// What the on-this-Mac card saves: the backend, and nothing else.
    ///
    /// No provider row is written, deliberately. The user has not chosen an
    /// online provider, and inventing one would put a service they never picked
    /// in front of any later switch back to online transcription. Setup still
    /// counts as complete, because `Settings::onboarding_complete` treats the
    /// local backend as an answer in its own right.
    fn save_local(&self) -> Result<(), String> {
        let ivars = self.ivars();
        let settings = ivars.engine.settings();
        settings.set("transcription_backend", "local")?;
        let controls = &ivars.controls;
        let index = controls.microphone.indexOfSelectedItem().max(0) as usize;
        let ids = controls.microphone_ids.borrow();
        settings.set(
            "microphone",
            ids.get(index).map(String::as_str).unwrap_or(""),
        )?;
        Ok(())
    }

    /// Setup is saved: end the sheet and leave the user on the main screen.
    ///
    /// The web wizard's last button says "Open my workspace" and lands on its
    /// main screen. This one handed the user to Settings instead, because
    /// until the main window existed there was no workspace to open. There is
    /// now, so the two agree again.
    fn finish(&self) {
        crate::ui::dismiss_sheet(&self.ivars().window, "onboarding");
        crate::app::with_app(|app| {
            app.with_settings(|page| page.reload());
            app.show_main(Some("dictate"));
        });
    }

    // ── Connection test ───────────────────────────────────

    fn request_models(&self) {
        let ivars = self.ivars();
        let (kind, url, key) = self.provider_fields();
        if let Err(error) = validate_provider(&kind, &url, &key) {
            self.set_text(&ivars.controls.connection_status, &error);
            return;
        }
        self.set_text(&ivars.controls.connection_status, "Checking access...");
        let engine = Arc::clone(&ivars.engine);
        let provider = join_provider(&kind, &url);
        let key = key.trim().to_string();
        let key = (!key.is_empty()).then_some(key);
        crate::app::spawn(async move {
            let result = engine.fetch_models(Some(provider), key).await;
            crate::events::on_main(move || {
                crate::app::with_app(|app| {
                    app.with_onboarding(|window| window.models_loaded(&result))
                });
            });
        });
    }

    fn models_loaded(&self, result: &Result<Vec<ModelInfo>, String>) {
        let ivars = self.ivars();
        let controls = &ivars.controls;
        match result {
            Ok(models) => {
                fill_combo(&controls.stt_model, models, "stt");
                fill_combo(&controls.chat_model, models, "chat");
                self.apply_provider_defaults();
                let mut connection = ivars.connection.get();
                connection.succeeded();
                ivars.connection.set(connection);
                let provider = option_for(&self.selected_provider()).label;
                self.set_text(
                    &controls.connection_status,
                    &format!(
                        "Connected to {}. Your key is valid and {} models are ready.",
                        provider,
                        models.len()
                    ),
                );
                self.set_text(&controls.error, "");
            }
            Err(error) => {
                let mut connection = ivars.connection.get();
                connection.invalidated();
                ivars.connection.set(connection);
                self.set_text(
                    &controls.connection_status,
                    &format!("Connection failed. {}", error),
                );
            }
        }
    }

    // ── Hotkey recorder ───────────────────────────────────

    fn start_recording_hotkey(&self) {
        self.stop_recording_hotkey();
        let ivars = self.ivars();
        ivars.recording.set(true);
        crate::app::with_app(|app| app.hotkeys().borrow_mut().suspend("record"));
        ivars
            .controls
            .hotkey
            .setTitle(&NSString::from_str("Press a shortcut..."));
        let this = self.retain();
        ivars
            .recorder
            .start(move |chord| this.finish_recording_hotkey(chord));
    }

    fn finish_recording_hotkey(&self, chord: Option<String>) {
        let ivars = self.ivars();
        if !ivars.recording.get() {
            return;
        }
        self.stop_recording_hotkey();
        let Some(chord) = chord else { return };
        let outcome = crate::app::with_app(|app| {
            app.hotkeys()
                .borrow_mut()
                .rebind(app.engine().settings(), "record", &chord)
        });
        match outcome {
            Some(Ok(())) => {
                ivars.controls.hotkey.setTitle(&NSString::from_str(&chord));
                // Settings shows the same binding; keep the two in step.
                crate::app::with_app(|app| app.with_settings(|window| window.reload()));
            }
            Some(Err(error)) => self.set_text(&ivars.controls.error, &error),
            None => {}
        }
    }

    fn stop_recording_hotkey(&self) {
        let ivars = self.ivars();
        ivars.recorder.stop();
        crate::app::with_app(|app| app.hotkeys().borrow_mut().resume());
        if ivars.recording.replace(false) {
            self.reload_hotkey();
        }
    }

    /// Forget whatever the last Test connection proved, and stop saying it.
    fn invalidate_connection(&self) {
        let ivars = self.ivars();
        let mut connection = ivars.connection.get();
        connection.invalidated();
        ivars.connection.set(connection);
        self.set_text(&ivars.controls.connection_status, "");
    }

    fn set_text(&self, field: &NSTextField, text: &str) {
        field.setStringValue(&NSString::from_str(text));
    }
}

// ── Value helpers ─────────────────────────────────────────

fn string_value(control: &NSControl) -> String {
    control.stringValue().to_string()
}

fn is_on(switch: &NSSwitch) -> bool {
    switch.state() == NSControlStateValueOn
}

fn set_switch(switch: &NSSwitch, on: bool) {
    switch.setState(if on {
        NSControlStateValueOn
    } else {
        NSControlStateValueOff
    });
}

fn bool_setting(on: bool) -> &'static str {
    if on {
        "true"
    } else {
        "false"
    }
}

/// Deepgram transcribes only, so it is not offered for cleanup. Same exclusion
/// the web wizard's formatting select makes.
fn formatting_options() -> Vec<&'static str> {
    PROVIDER_OPTIONS
        .iter()
        .filter(|option| option.value != "deepgram")
        .map(|option| option.value)
        .collect()
}

fn select_provider_popup(popup: &NSPopUpButton, kind: &str) {
    let index = formatting_options()
        .iter()
        .position(|value| *value == kind)
        .unwrap_or(0);
    popup.selectItemAtIndex(index as isize);
}

fn fill_combo(combo: &NSComboBox, models: &[ModelInfo], kind: &str) {
    unsafe {
        combo.removeAllItems();
        for model in models.iter().filter(|model| model.model_type == kind) {
            combo.addItemWithObjectValue(&NSString::from_str(&model.id));
        }
    }
}

// ── Panel construction ────────────────────────────────────

fn build_panels(mtm: MainThreadMarker) -> (Retained<NSTabView>, Controls) {
    let panels = NSTabView::initWithFrame(
        NSTabView::alloc(mtm),
        NSRect::new(
            NSPoint::new(16.0, 72.0),
            NSSize::new(STEP_WIDTH, STEP_HEIGHT),
        ),
    );
    // A wizard, not a tab bar: the header and the buttons are the navigation.
    panels.setTabViewType(NSTabViewType::NoTabsNoBorder);

    let welcome_view = build_welcome(mtm);
    let (provider_view, providers, local_card, same_provider, formatting_provider) =
        build_provider(mtm);
    let (credentials_view, provider_url, api_key, connection_status, test) = build_credentials(mtm);
    let (preferences_view, stt_model, chat_model, microphone, refresh, hotkey) =
        build_preferences(mtm);
    let (done_view, summary) = build_done(mtm);

    for (title, view) in [
        ("Welcome", &welcome_view),
        ("Provider", &provider_view),
        ("Connection", &credentials_view),
        ("Preferences", &preferences_view),
        ("Done", &done_view),
    ] {
        let item = unsafe {
            NSTabViewItem::initWithIdentifier(
                NSTabViewItem::alloc(),
                Some(&NSString::from_str(title)),
            )
        };
        item.setLabel(&NSString::from_str(title));
        item.setView(Some(view));
        panels.addTabViewItem(&item);
    }

    let kicker = note(
        mtm,
        "",
        NSRect::new(
            NSPoint::new(20.0, WINDOW_HEIGHT - 34.0),
            NSSize::new(STEP_WIDTH, 14.0),
        ),
    );
    let heading = NSTextField::labelWithString(&NSString::from_str(""), mtm);
    heading.setFrame(NSRect::new(
        NSPoint::new(20.0, WINDOW_HEIGHT - 60.0),
        NSSize::new(STEP_WIDTH, 22.0),
    ));
    heading.setFont(Some(&objc2_app_kit::NSFont::boldSystemFontOfSize(15.0)));

    let error = note(
        mtm,
        "",
        NSRect::new(NSPoint::new(20.0, 48.0), NSSize::new(STEP_WIDTH, 16.0)),
    );
    error.setTextColor(Some(&objc2_app_kit::NSColor::systemRedColor()));

    let back = button(
        mtm,
        NSRect::new(NSPoint::new(20.0, 12.0), NSSize::new(90.0, 28.0)),
        "Back",
        0,
    );
    // A sheet draws no close button, and `performClose:` is refused on one, so
    // Cmd+W is not a way out either -- without this the wizard is a room with
    // no door. Escape is the gesture a sheet is supposed to answer, and the
    // button says so out loud rather than leaving it to be guessed.
    let later = button(
        mtm,
        NSRect::new(NSPoint::new(118.0, 12.0), NSSize::new(130.0, 28.0)),
        "Do this later",
        0,
    );
    later.setKeyEquivalent(&NSString::from_str("\u{1b}"));

    let primary = button(
        mtm,
        NSRect::new(
            NSPoint::new(WINDOW_WIDTH - 20.0 - 190.0, 12.0),
            NSSize::new(190.0, 28.0),
        ),
        "Get started",
        0,
    );
    primary.setKeyEquivalent(&NSString::from_str("\r"));

    let controls = Controls {
        kicker,
        heading,
        error,
        back,
        later,
        primary,
        providers,
        local_card,
        same_provider,
        formatting_provider,
        provider_url,
        api_key,
        connection_status,
        test,
        stt_model,
        chat_model,
        microphone,
        microphone_ids: RefCell::new(vec![String::new()]),
        refresh,
        hotkey,
        summary,
    };
    (panels, controls)
}

fn build_welcome(mtm: MainThreadMarker) -> Retained<NSView> {
    let mut form = Form::new(mtm, STEP_WIDTH, STEP_HEIGHT);
    let frame = form.full(44.0);
    form.add(&note(
        mtm,
        "Hold a shortcut, speak naturally, and polished text lands where you are working.",
        frame,
    ));
    let frame = form.full(44.0);
    form.add(&note(
        mtm,
        "Your key stays on this device, in the macOS keychain. Audio goes only to the provider you choose.",
        frame,
    ));
    let frame = form.full(44.0);
    form.add(&note(
        mtm,
        "Setup takes three panels: pick a provider, connect it, and choose a microphone and a shortcut.",
        frame,
    ));
    form.view.clone()
}

#[allow(clippy::type_complexity)]
fn build_provider(
    mtm: MainThreadMarker,
) -> (
    Retained<NSView>,
    Vec<Retained<NSButton>>,
    Retained<NSButton>,
    Retained<NSSwitch>,
    Retained<NSPopUpButton>,
) {
    let mut form = Form::new(mtm, STEP_WIDTH, STEP_HEIGHT);
    let frame = form.full(28.0);
    form.add(&note(
        mtm,
        "Groq is the fastest path: one key covers Whisper transcription, cleanup, and Orpheus voice.",
        frame,
    ));

    let mut providers = Vec::new();
    for (index, option) in PROVIDER_OPTIONS.iter().enumerate() {
        let frame = form.full(18.0);
        let button = radio(
            mtm,
            NSRect::new(frame.origin, NSSize::new(STEP_WIDTH - 110.0, 18.0)),
            option.label,
            TAG_PROVIDER_BASE + index as isize,
        );
        form.add(&button);
        if let Some(badge) = badge_text(option) {
            let badge_frame = NSRect::new(
                NSPoint::new(STEP_WIDTH - 100.0, frame.origin.y),
                NSSize::new(100.0, 16.0),
            );
            form.add(&note(mtm, badge, badge_frame));
        }
        providers.push(button);
        let frame = form.full(14.0);
        form.add(&note(
            mtm,
            option.description,
            NSRect::new(
                NSPoint::new(frame.origin.x + 20.0, frame.origin.y),
                NSSize::new(STEP_WIDTH - 20.0 - frame.origin.x, 14.0),
            ),
        ));
    }

    // The on-this-Mac card: last, and in the same radio group, because it is an
    // answer to the same question. It carries no key and no endpoint, so
    // choosing it ends setup on this panel.
    let frame = form.full(18.0);
    let local_card = radio(
        mtm,
        NSRect::new(frame.origin, NSSize::new(STEP_WIDTH - 110.0, 18.0)),
        LOCAL_CARD_LABEL,
        TAG_LOCAL_CARD,
    );
    form.add(&local_card);
    // The sentence under the card is 607pt of text in a 468pt column, in a row
    // that was tall enough for two lines but never had wrapping turned on. So
    // it drew as one line and was cut mid-word: "...No key, no network, and no
    // audio leaves the machine. Needs Pyth". Wrapped, it is 26pt -- which is
    // what the 28 was guessing at -- so the row is measured rather than guessed.
    let note_x = frame.origin.x + 20.0;
    let note_width = STEP_WIDTH - 20.0 - frame.origin.x;
    let local_note = note(
        mtm,
        LOCAL_CARD_DESCRIPTION,
        NSRect::new(NSPoint::new(note_x, 0.0), NSSize::new(note_width, 14.0)),
    );
    wrap(&local_note, note_width);
    let frame = form.full(local_note.frame().size.height);
    local_note.setFrameOrigin(NSPoint::new(note_x, frame.origin.y));
    form.add(&local_note);

    let (l, c) = form.row(ROW);
    form.add(&label(mtm, "Same for cleanup", l));
    let same_provider = switch_control(
        mtm,
        NSRect::new(c.origin, NSSize::new(38.0, c.size.height)),
        TAG_SAME_PROVIDER,
    );
    form.add(&same_provider);

    let (l, c) = form.row(ROW);
    form.add(&label(mtm, "Cleanup provider", l));
    let titles: Vec<&str> = formatting_options()
        .into_iter()
        .map(|value| option_for(value).label)
        .collect();
    let formatting_provider = popup(mtm, c, TAG_FORMATTING_PROVIDER, &titles);
    form.add(&formatting_provider);
    form.note_row(
        mtm,
        "Deepgram handles speech only; it cannot clean text up.",
    );

    (
        form.view.clone(),
        providers,
        local_card,
        same_provider,
        formatting_provider,
    )
}

#[allow(clippy::type_complexity)]
fn build_credentials(
    mtm: MainThreadMarker,
) -> (
    Retained<NSView>,
    Retained<NSTextField>,
    Retained<NSSecureTextField>,
    Retained<NSTextField>,
    Retained<NSButton>,
) {
    let mut form = Form::new(mtm, STEP_WIDTH, STEP_HEIGHT);
    let frame = form.full(28.0);
    form.add(&note(
        mtm,
        "We check access before saving anything. OpenFlow never sends your key anywhere else.",
        frame,
    ));

    let (l, c) = form.row(ROW);
    form.add(&label(mtm, "Endpoint URL", l));
    let provider_url = text_field(mtm, c, 0);
    form.add(&provider_url);
    form.note_row(mtm, "Only used by Self-hosted / LAN, and required for it.");

    let (l, c) = form.row(ROW);
    form.add(&label(mtm, "API key", l));
    let api_key = secure_field(mtm, c, 0);
    form.add(&api_key);
    form.note_row(mtm, "Stored in the macOS keychain, never in the database. A self-hosted endpoint may leave this empty.");

    let frame = form.control_only(ROW);
    let test = button(
        mtm,
        NSRect::new(frame.origin, NSSize::new(150.0, frame.size.height)),
        "Test connection",
        0,
    );
    form.add(&test);
    let frame = form.full(40.0);
    let connection_status = note(mtm, "", frame);
    form.add(&connection_status);

    (
        form.view.clone(),
        provider_url,
        api_key,
        connection_status,
        test,
    )
}

#[allow(clippy::type_complexity)]
fn build_preferences(
    mtm: MainThreadMarker,
) -> (
    Retained<NSView>,
    Retained<NSComboBox>,
    Retained<NSComboBox>,
    Retained<NSPopUpButton>,
    Retained<NSButton>,
    Retained<NSButton>,
) {
    let mut form = Form::new(mtm, STEP_WIDTH, STEP_HEIGHT);
    let frame = form.full(28.0);
    form.add(&note(
        mtm,
        "Smart defaults are ready. Change a model now or paste any compatible model id later.",
        frame,
    ));

    let (l, c) = form.row(ROW);
    form.add(&label(mtm, "Speech-to-text model", l));
    let stt_model = combo(mtm, c, 0);
    form.add(&stt_model);

    let (l, c) = form.row(ROW);
    form.add(&label(mtm, "Cleanup model", l));
    let chat_model = combo(mtm, c, 0);
    form.add(&chat_model);

    let (l, c) = form.row(ROW);
    form.add(&label(mtm, "Microphone", l));
    let microphone = popup(
        mtm,
        NSRect::new(c.origin, NSSize::new(c.size.width - 90.0, c.size.height)),
        0,
        &[],
    );
    form.add(&microphone);
    let refresh = button(
        mtm,
        NSRect::new(
            NSPoint::new(c.origin.x + c.size.width - 84.0, c.origin.y),
            NSSize::new(84.0, c.size.height),
        ),
        "Refresh",
        0,
    );
    form.add(&refresh);

    let (l, c) = form.row(ROW);
    form.add(&label(mtm, "Record shortcut", l));
    let hotkey = button(mtm, c, "Option+V", 0);
    form.add(&hotkey);
    form.note_row(
        mtm,
        "Click, then press the chord. Hold it to record, release it to transcribe.",
    );

    (
        form.view.clone(),
        stt_model,
        chat_model,
        microphone,
        refresh,
        hotkey,
    )
}

fn build_done(mtm: MainThreadMarker) -> (Retained<NSView>, Retained<NSTextField>) {
    let mut form = Form::new(mtm, STEP_WIDTH, STEP_HEIGHT);
    let frame = form.full(28.0);
    form.add(&note(
        mtm,
        "Setup is saved. Here is what OpenFlow will use:",
        frame,
    ));
    let frame = form.full(40.0);
    let summary = note(mtm, "", frame);
    form.add(&summary);
    let frame = form.full(44.0);
    form.add(&note(
        mtm,
        "OpenFlow lives in the menu bar. Open Settings from there any time, and find past dictations under History.",
        frame,
    ));
    (form.view.clone(), summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wizard's captions are one line each, and have to stay one line.
    ///
    /// `build_provider` gives each one a fixed 14pt row and never turns
    /// wrapping on, so a caption too wide for its column is not wrapped -- it is
    /// cut at the edge. That is how the on-this-Mac card came to read
    /// "...No key, no network, and no audio leaves the machine. Needs Pyth".
    ///
    /// That one wraps now and is measured. These five deliberately do not: the
    /// panel they sit in is `STEP_HEIGHT`, which is the measured height of the
    /// tallest panel, and a caption that grew to two lines would push the panel
    /// past it and back on top of the Back and Continue buttons. So the
    /// constraint here is real, and this is where it is written down.
    #[test]
    fn every_provider_caption_stays_on_one_line() {
        let column = STEP_WIDTH - 20.0;
        for option in PROVIDER_OPTIONS {
            let width = crate::ui::metrics::note_width(option.description);
            assert!(
                width <= column,
                "{:?}'s caption renders {width:.1}pt wide in a {column}pt row that does not wrap; \
                 shorten it, or give it the wrapping treatment the on-this-Mac card got and check \
                 STEP_HEIGHT still holds the panel",
                option.label
            );
        }
    }

    /// The on-this-Mac card's own two strings, which sit in different boxes: the
    /// radio's title shares its row with the badge column, and the caption wraps
    /// into the same column as the ones above.
    #[test]
    fn the_on_this_mac_card_fits_its_row() {
        let title = crate::ui::metrics::label_width(LOCAL_CARD_LABEL);
        let room = STEP_WIDTH - 110.0;
        assert!(
            title <= room,
            "the card title {LOCAL_CARD_LABEL:?} renders {title:.1}pt wide in {room}pt"
        );
    }

    /// The wizard must not be able to walk off either end, and every panel has
    /// to be reachable from the front by pressing the primary button.
    #[test]
    fn the_step_machine_saturates_at_both_ends() {
        assert_eq!(Step::Welcome.back(), Step::Welcome);
        assert_eq!(Step::Done.next(), Step::Done);

        let mut step = Step::Welcome;
        let mut seen = vec![step];
        for _ in 0..Step::ORDER.len() {
            step = step.next();
            if *seen.last().unwrap() != step {
                seen.push(step);
            }
        }
        assert_eq!(seen, Step::ORDER.to_vec());

        // Back undoes Next everywhere Next actually moves. The last panel is
        // excluded on purpose: `next` saturates there, so undoing it would mean
        // walking backwards off a step the user never took.
        for step in Step::ORDER.iter().take(Step::ORDER.len() - 1) {
            assert_eq!(step.next().back(), *step, "{step:?} must be reversible");
        }
    }

    /// The kicker counts panels the way the web wizard counts steps, from one.
    #[test]
    fn the_kicker_counts_from_one() {
        assert_eq!(Step::Welcome.kicker(), "Setup · 1 of 5");
        assert_eq!(Step::Done.kicker(), "Setup · 5 of 5");
    }

    /// The web build's rule: a key is required for every hosted provider, and a
    /// custom endpoint needs a whole URL but may have no key at all.
    #[test]
    fn an_empty_key_is_only_valid_for_a_custom_endpoint() {
        assert!(validate_provider("groq", "", "gsk-test").is_ok());
        assert_eq!(
            validate_provider("groq", "", "   ").unwrap_err(),
            "Enter an API key to continue."
        );
        assert!(validate_provider("custom", "http://192.168.1.10:8880/v1", "").is_ok());
        assert!(validate_provider("custom", "HTTPS://box.lan/v1", "").is_ok());
        assert_eq!(
            validate_provider("custom", "box.lan/v1", "").unwrap_err(),
            "Enter a complete endpoint URL beginning with http:// or https://."
        );
        assert!(validate_provider("openai", "", "sk-test").is_ok());
    }

    /// Setup may not be finished on an unverified key, and the credentials
    /// panel may not be left on one either; the other panels never block.
    #[test]
    fn the_connection_gate_holds_the_credentials_panel() {
        assert!(can_advance(Step::Welcome, "groq", "", "", false).is_ok());
        assert!(can_advance(Step::Provider, "groq", "", "", false).is_ok());
        assert_eq!(
            can_advance(Step::Credentials, "groq", "", "gsk-test", false).unwrap_err(),
            "Test the connection before continuing."
        );
        assert!(can_advance(Step::Credentials, "groq", "", "gsk-test", true).is_ok());
        assert_eq!(
            can_advance(Step::Credentials, "groq", "", "", true).unwrap_err(),
            "Enter an API key to continue."
        );
        // Finishing re-checks the fields, because the user can go back and
        // clear the key after the connection succeeded.
        assert_eq!(
            can_advance(Step::Preferences, "groq", "", "", true).unwrap_err(),
            "Enter an API key to continue."
        );
    }

    /// Editing a credential after a successful test has to close the gate
    /// again. This is what the first round missed: the window proved one key,
    /// the user pasted a different one over it, and setup saved a key nothing
    /// had ever called.
    #[test]
    fn editing_a_credential_after_a_successful_test_re_closes_the_gate() {
        let gate = |connection: Connection| {
            can_advance(
                Step::Credentials,
                "groq",
                "",
                "gsk-test",
                connection.proven(),
            )
        };

        let mut connection = Connection::default();
        assert!(!connection.proven(), "nothing is proven before a test");
        assert!(gate(connection).is_err());

        connection.succeeded();
        assert!(gate(connection).is_ok(), "a proven key opens the gate");

        // What `controlTextDidChange:` does on either credential field.
        connection.invalidated();
        assert_eq!(
            gate(connection).unwrap_err(),
            "Test the connection before continuing.",
            "an edited credential must be tested again"
        );

        // Proving it again reopens the gate, and editing closes it again: the
        // state is a latch, not a one-way flag.
        connection.succeeded();
        assert!(gate(connection).is_ok());
        connection.invalidated();
        assert!(gate(connection).is_err());
    }

    /// Groq leads the list and is the only badged option, as in the web grid,
    /// and the cleanup list drops Deepgram.
    #[test]
    fn the_provider_list_matches_the_web_grid() {
        assert_eq!(PROVIDER_OPTIONS[0].value, "groq");
        assert_eq!(badge_text(&PROVIDER_OPTIONS[0]), Some("Recommended"));
        assert_eq!(
            PROVIDER_OPTIONS
                .iter()
                .filter(|option| option.recommended)
                .count(),
            1
        );
        assert_eq!(PROVIDER_OPTIONS.last().unwrap().value, "custom");
        assert!(!formatting_options().contains(&"deepgram"));
        assert_eq!(formatting_options().len(), PROVIDER_OPTIONS.len() - 1);
        // An unknown stored provider falls back to the shipped default rather
        // than to whatever happens to be first in a future edit.
        assert_eq!(
            option_for("nonesuch").value,
            openflow_core::settings::DEFAULT_PROVIDER
        );
    }

    /// The on-this-Mac card answers the same question as the provider grid but
    /// has no credential to prove, so every gate that exists to protect a key
    /// has to let it through. Without this, choosing it would stall on "Enter
    /// an API key to continue" for a backend that has no key.
    #[test]
    fn the_on_this_mac_card_needs_no_key_and_no_endpoint() {
        assert!(is_local_card(LOCAL_CARD));
        assert!(!is_local_card("groq"));
        assert!(!is_local_card("custom"));
        assert!(
            !PROVIDER_OPTIONS
                .iter()
                .any(|option| option.value == LOCAL_CARD),
            "the card is a backend, not a provider, and must not be in the grid"
        );

        assert!(validate_provider(LOCAL_CARD, "", "").is_ok());
        for step in Step::ORDER {
            assert!(
                can_advance(step, LOCAL_CARD, "", "", false).is_ok(),
                "{step:?} must not hold the on-this-Mac card on an untested key"
            );
        }
        // ...and the gates still hold for everything else.
        assert!(can_advance(Step::Credentials, "groq", "", "", false).is_err());
    }

    /// The defaults the web wizard shows as placeholders, per provider.
    #[test]
    fn every_provider_offers_a_default_model_pair() {
        assert_eq!(option_for("groq").stt_default, "whisper-large-v3-turbo");
        assert_eq!(option_for("groq").chat_default, "openai/gpt-oss-20b");
        assert_eq!(option_for("openai").stt_default, "whisper-1");
        assert_eq!(option_for("deepgram").stt_default, "nova-3");
        for option in PROVIDER_OPTIONS {
            assert!(!option.stt_default.is_empty(), "{}", option.value);
            assert!(!option.chat_default.is_empty(), "{}", option.value);
        }
    }

    /// The closing summary names the four things setup decided, and falls back
    /// to the provider's own model when the field was left empty.
    #[test]
    fn the_summary_names_what_was_saved() {
        assert_eq!(
            summary_line(
                "groq",
                "whisper-large-v3",
                "MacBook Pro Microphone",
                "Option+V"
            ),
            "Groq · whisper-large-v3 · MacBook Pro Microphone · hold Option+V to dictate, active now"
        );
        assert_eq!(
            summary_line("groq", "  ", "System default", "Option+V"),
            "Groq · whisper-large-v3-turbo · System default · hold Option+V to dictate, active now"
        );
    }
}
