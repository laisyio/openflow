//!
//! Presented as a sheet on the main window rather than as a window of its own.
//! It is still an `NSWindow` -- a sheet is -- but it hangs off the one window
//! the app now has, so setup happens over the workspace it is setting up
//! instead of beside it. `Closable` is kept even though a sheet draws no close
//! button: `performClose:` still runs, so the Cmd+W in the app menu is still
//! the way out of a wizard the user does not want to finish.
//! First-run setup: the native form of `App.tsx`'s onboarding screen.
//!
//! Cloud setup has five panels; private setup skips the credential panel but
//! keeps microphone, shortcut and history preferences. The final panel reports
//! saved configuration, not permission or model readiness. Private setup hands
//! off to the existing runner installer and model downloader in Settings.
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

use openflow_core::audio::AudioDevice;
use openflow_core::engine::Engine;
use openflow_core::transcribe::ModelInfo;

use crate::ui::recorder::ChordRecorder;
use crate::ui::settings::{join_provider, provider_is_loopback, split_provider};
use crate::ui::{
    allow_wrapping, button, combo, label, note, popup, radio, secure_field, switch_control,
    text_field, wire, Form, ROW,
};

const WINDOW_WIDTH: f64 = 640.0;
/// Fixed-size sheet with a separate heading and navigation region. Form does
/// not clip overflowing rows: the snapshot/layout checks must cover all steps.
const WINDOW_HEIGHT: f64 = STEP_HEIGHT + 164.0;
const STEP_WIDTH: f64 = WINDOW_WIDTH - 48.0;
const STEP_HEIGHT: f64 = 500.0;

/// The box the line under the microphone picker gets.
///
/// Filled in by `reload_microphones`, so the frame is reserved rather than
/// fitted: two lines of [`note`] text, because the longest thing this line says
/// is an enumeration error with the audio thread's own words inside it, and a
/// label sized to the empty placeholder it is built with would have nowhere to
/// put one. Nothing clips it -- `allow_wrapping` turns wrapping on and stops
/// there -- so a third line does not truncate, it draws over the Record
/// shortcut row underneath. It is a named constant because the arithmetic that
/// makes two lines enough is a claim about the sentences in
/// [`microphone_note`], and that claim is asserted rather than remembered; see
/// `every_microphone_note_fits_the_line_reserved_for_it`.
const MICROPHONE_NOTE_HEIGHT: f64 = 28.0;

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
            Step::Welcome => "Less typing. More flow.",
            Step::Provider => "Where should your words go?",
            Step::Credentials => "Connect your provider",
            Step::Preferences => "Make it yours",
            Step::Done => "Your preferences are saved",
        }
    }

    /// What the primary button says on this panel.
    pub fn primary_title(self) -> &'static str {
        match self {
            Step::Welcome => "Get started",
            Step::Provider => "Continue to connection",
            Step::Credentials => "Continue to preferences",
            Step::Preferences => "Finish setup",
            Step::Done => "Open my workspace",
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
/// radio and skips only the cloud credential panel.
pub const LOCAL_CARD: &str = "local";
/// The card's title and the sentence under it.
pub const LOCAL_CARD_LABEL: &str = "On this Mac (private)";
pub const LOCAL_CARD_DESCRIPTION: &str =
    "No API key. Speech stays on your Mac. Requires Python and a one-time model download; works offline after setup.";

/// Local setup skips credentials, never microphone and privacy preferences.
fn next_step(step: Step, local: bool) -> Step {
    if step == Step::Provider && local {
        Step::Preferences
    } else {
        step.next()
    }
}

fn previous_step(step: Step, local: bool) -> Step {
    if step == Step::Preferences && local {
        Step::Provider
    } else {
        step.back()
    }
}

fn progress_text(step: Step, local: bool) -> String {
    if local && step != Step::Credentials {
        let position = match step {
            Step::Welcome => 1,
            Step::Provider => 2,
            Step::Preferences => 3,
            _ => 4,
        };
        format!("SET UP OPENFLOW  ·  {position} OF 4")
    } else {
        format!("SET UP OPENFLOW  ·  {} OF 5", step.index() + 1)
    }
}

const PRIVATE_INTRO: &str = "Choose your microphone and history preference. Next, install the private engine in Settings → Providers. Cloud cleanup is turned off; no API key is needed.";
const PRIVATE_SUMMARY: &str = "1   Install the engine in Settings → Providers.\n\n2   Download a speech model there.\n\n3   Wait for both to report ready, then try a short phrase.\n\nSpeech stays on your Mac. Downloads need internet. Enabled plugins are separate programs, not confined by Local-only protection; review them in Plugins.";
const CLOUD_BLOCKED_MESSAGE: &str = "Local-only protection is enabled. In Settings → Providers, select On this Mac to reveal Local-only, then turn that protection off explicitly before returning to cloud setup.";

fn connection_allowed(local_only: bool, provider: &str) -> bool {
    !local_only || provider_is_loopback(provider)
}

fn cleanup_setting(local: bool, requested: bool) -> &'static str {
    bool_setting(!local && requested)
}

/// A model-list check proves only the transcription endpoint and its key.
/// Never treat a different cleanup endpoint as tested by that same request.
fn cleanup_verified(
    stt_kind: &str,
    stt_url: &str,
    cleanup_kind: &str,
    cleanup_url: &str,
    connected: bool,
) -> bool {
    connected
        && !is_local_card(stt_kind)
        && crate::ui::settings::serves_cleanup(cleanup_kind)
        && join_provider(stt_kind, stt_url).trim_end_matches('/')
            == join_provider(cleanup_kind, cleanup_url).trim_end_matches('/')
}

const CLEANUP_DISCLOSURE: &str =
    "Optional: sends transcript text to your cleanup provider for rewriting.";
const SEPARATE_CLEANUP_DISCLOSURE: &str =
    "Cleanup is off. Set up a separate cleanup provider in Settings → Providers.";

fn finish_destination(local: bool) -> &'static str {
    if local {
        "providers"
    } else {
        "dictate"
    }
}

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
    if microphone == NO_MICROPHONE {
        return format!(
            "{} · {} · {} · connect an input device, then hold {} to dictate",
            provider, model, microphone, shortcut
        );
    }
    format!(
        "{} · {} · {} · shortcut: {}. Microphone and Accessibility permission may still be needed.",
        provider, model, microphone, shortcut
    )
}

/// What the microphone popup shows when the wizard could not name a device.
///
/// It is a device name, not a message, because it is also what the closing
/// panel reads back out of the popup: whatever stands here is what setup
/// claims OpenFlow will record with.
pub const NO_MICROPHONE: &str = "No microphone detected";

/// The popup's items for `devices`, in the order the id list is built in.
///
/// "System default" used to be added unconditionally, before the devices and
/// whatever the enumeration had said. That named a device that need not exist:
/// `list_audio_devices` fails one way and comes back empty another -- a wedged
/// CoreAudio HAL times out after two seconds (audio.rs:275), while an
/// enumeration error or a machine with no input at all just yields nothing
/// (audio.rs:336) -- and `unwrap_or_default` flattened both into the same empty
/// list. The fallback the item promises does not exist either: `Start` resolves
/// an empty selection through `default_input_device`, and when that is `None`
/// the first real dictation fails with "No microphone found. Connect or enable
/// an input device." (audio.rs:126-131). So an empty list says so here, in the
/// words the web wizard uses for the same state (src/App.tsx:1026).
pub fn microphone_items(devices: &[AudioDevice]) -> Vec<String> {
    if devices.is_empty() {
        return vec![NO_MICROPHONE.to_string()];
    }
    let mut items = vec!["System default".to_string()];
    items.extend(devices.iter().map(|device| {
        if device.is_default {
            format!("{} (default)", device.name)
        } else {
            device.name.clone()
        }
    }));
    items
}

/// The line under the picker: how many devices were found, or why none were.
///
/// The two failures are kept apart on purpose, because the user's next move
/// differs. Nothing enumerated is usually the microphone grant, which Refresh
/// picks up once it is given; an `Err` is the audio thread not answering, which
/// Refresh retries. The web wizard shows the first of these and nothing for the
/// second, since `invoke` rejections land in its error banner instead.
pub fn microphone_note(listed: Result<usize, &str>) -> String {
    match listed {
        Err(error) => format!("Could not read microphones: {error}. Press Refresh to try again."),
        Ok(0) => {
            "No microphone detected yet. Check system permission, then press Refresh.".to_string()
        }
        Ok(1) => "1 microphone ready.".to_string(),
        Ok(count) => format!("{count} microphones ready."),
    }
}

/// The closing panel's heading, which [`Step::title`] cannot answer alone.
///
/// "OpenFlow is ready" is a promise about the whole pipeline, and setup is only
/// four fifths of it: everything else the wizard collected can be correct while
/// the one thing it cannot supply -- a microphone -- is missing. Saying it here
/// costs a panel the user reads anyway, rather than at the first hotkey press.
pub fn done_heading(microphone: &str) -> &'static str {
    if microphone == NO_MICROPHONE {
        return "Saved. Connect a microphone next.";
    }
    Step::Done.title()
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
    microphone_note: Retained<NSTextField>,
    microphone_ids: RefCell<Vec<String>>,
    refresh: Retained<NSButton>,
    hotkey: Retained<NSButton>,
    save_history: Retained<NSSwitch>,
    preferences_intro: Retained<NSTextField>,
    model_labels: Vec<Retained<NSTextField>>,
    format_enabled: Retained<NSSwitch>,
    cleanup_note: Retained<NSTextField>,
    done_support: Vec<Retained<NSTextField>>,

    summary: Retained<NSTextField>,
}

pub struct OnboardingIvars {
    engine: Arc<Engine>,
    window: Retained<NSWindow>,
    panels: Retained<NSTabView>,
    controls: Controls,
    step: Cell<Step>,
    connection: Cell<Connection>,
    connection_generation: Cell<u64>,
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
            self.dismiss();
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
            self.show_step(previous_step(step, is_local_card(&self.selected_provider())));
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
            self.dismiss();
        }

        #[unsafe(method(providerChanged:))]
        fn provider_changed(&self, _sender: &NSControl) {
            // A different provider is a different key and a different model
            // list, so the connection has to be proven again.
            self.invalidate_connection();
            // Replace shipped defaults when changing providers, but preserve
            // explicitly entered model identifiers.
            let controls = &self.ivars().controls;
            let selected = self.selected_provider();
            let option = option_for(&selected);
            let speech = string_value(&controls.stt_model);
            if should_replace_default(&speech, "stt") {
                controls.stt_model.setStringValue(&NSString::from_str(option.stt_default));
            }
            let cleanup = string_value(&controls.chat_model);
            if should_replace_default(&cleanup, "chat") {
                controls.chat_model.setStringValue(&NSString::from_str(option.chat_default));
            }
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
            connection_generation: Cell::new(0),
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
        set_switch(&controls.save_history, settings.save_history());
        set_switch(&controls.format_enabled, settings.format_enabled());
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
        ivars
            .connection_generation
            .set(ivars.connection_generation.get().wrapping_add(1));
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
        let listed = ivars.engine.list_audio_devices();
        self.set_text(
            &controls.microphone_note,
            &microphone_note(listed.as_ref().map(Vec::len).map_err(String::as_str)),
        );
        let devices = listed.unwrap_or_default();
        // The id list stays keyed to the popup by position, and its first entry
        // is the empty string either way: an empty `microphone` row means "let
        // the recorder pick", which is still the right thing to save when there
        // was nothing to enumerate.
        let mut ids = vec![String::new()];
        controls.microphone.removeAllItems();
        for title in microphone_items(&devices) {
            controls
                .microphone
                .addItemWithTitle(&NSString::from_str(&title));
        }
        ids.extend(devices.iter().map(|device| device.id.clone()));
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
        let local = is_local_card(&self.selected_provider());
        self.set_text(&controls.kicker, &progress_text(step, local));
        let heading = if step == Step::Done && local {
            "Next: install your private engine"
        } else if step == Step::Done {
            done_heading(&self.selected_microphone())
        } else {
            step.title()
        };
        self.set_text(&controls.heading, heading);
        controls
            .primary
            .setTitle(&NSString::from_str(step.primary_title()));
        controls
            .back
            .setHidden(step == Step::Welcome || step == Step::Done);
        controls.later.setHidden(step == Step::Done);
        controls.stt_model.setEnabled(!local);
        controls.chat_model.setEnabled(!local);
        configure_private_preferences(controls, local);
        configure_done(controls, local);
        self.set_text(&controls.preferences_intro, if local {
            PRIVATE_INTRO
        } else {
            "Defaults are filled in for you. Microphone permission lets OpenFlow hear you; Accessibility permission lets it insert text into other apps."
        });
        if local && step == Step::Done {
            controls
                .primary
                .setTitle(&NSString::from_str("Continue to local setup"));
        }

        // Deepgram transcribes only, so one provider cannot serve both. The web
        // wizard disables the toggle for it (src/App.tsx:934); here it is
        // switched off as well, or "same" would mean "clean up with a provider
        // that cannot".
        let kind = self.selected_provider();
        if step == Step::Provider && is_local_card(&kind) {
            controls
                .primary
                .setTitle(&NSString::from_str("Continue privately"));
        }
        let deepgram = kind == "deepgram";
        if deepgram && is_on(&controls.same_provider) {
            set_switch(&controls.same_provider, false);
        }
        controls.same_provider.setEnabled(!deepgram && !local);

        // The web wizard hides the cleanup provider behind the same-provider
        // toggle; here it is present and inert, so the panel does not reflow.
        // While it is inert it shows the provider that will actually be used,
        // which is the transcription provider, because that is what `save`
        // stores.
        let same = is_on(&controls.same_provider);
        if same {
            select_provider_popup(&controls.formatting_provider, &kind);
        }
        controls.formatting_provider.setEnabled(!same && !local);
        controls.provider_url.setEnabled(kind == "custom");
        let cleanup_allowed = self.cleanup_is_verified();
        controls
            .format_enabled
            .setEnabled(cleanup_allowed && !local);
        controls.chat_model.setEnabled(cleanup_allowed && !local);
        if !cleanup_allowed && matches!(step, Step::Preferences | Step::Done) {
            set_switch(&controls.format_enabled, false);
        }
        self.set_text(
            &controls.cleanup_note,
            if cleanup_allowed {
                CLEANUP_DISCLOSURE
            } else {
                SEPARATE_CLEANUP_DISCLOSURE
            },
        );
    }

    fn advance(&self) {
        let ivars = self.ivars();
        let step = ivars.step.get();
        if step == Step::Done {
            self.finish();
            return;
        }
        let local = is_local_card(&self.selected_provider());
        let (kind, url, key) = self.provider_fields();
        let proven = ivars.connection.get().proven();
        if let Err(error) = can_advance(step, &kind, &url, &key, proven) {
            self.set_text(&ivars.controls.error, &error);
            return;
        }
        if step == Step::Preferences {
            if let Err(error) = if local {
                self.save_local()
            } else {
                self.save()
            } {
                self.set_text(&ivars.controls.error, &error);
                return;
            }
        }
        self.show_step(next_step(step, local));
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

    fn selected_cleanup_provider(&self) -> String {
        let controls = &self.ivars().controls;
        if is_on(&controls.same_provider) {
            return self.selected_provider();
        }
        let index = controls.formatting_provider.indexOfSelectedItem().max(0) as usize;
        formatting_options()
            .get(index)
            .copied()
            .unwrap_or("groq")
            .to_string()
    }

    fn cleanup_is_verified(&self) -> bool {
        let (kind, url, _) = self.provider_fields();
        cleanup_verified(
            &kind,
            &url,
            &self.selected_cleanup_provider(),
            &url,
            self.ivars().connection.get().proven(),
        )
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

    /// Whatever the picker is currently claiming OpenFlow will record with.
    fn selected_microphone(&self) -> String {
        self.ivars()
            .controls
            .microphone
            .titleOfSelectedItem()
            .map(|title| title.to_string())
            .unwrap_or_else(|| "System default".to_string())
    }

    fn fill_summary(&self) {
        let ivars = self.ivars();
        let controls = &ivars.controls;
        let microphone = self.selected_microphone();
        let shortcut = controls.hotkey.title().to_string();
        if is_local_card(&self.selected_provider()) {
            self.set_text(&controls.summary, PRIVATE_SUMMARY);
            return;
        }
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
        can_advance(
            Step::Credentials,
            &kind,
            &url,
            &key,
            ivars.connection.get().proven(),
        )?;
        settings.set("save_history", bool_setting(is_on(&controls.save_history)))?;
        settings.set(
            "format_enabled",
            cleanup_setting(
                false,
                self.cleanup_is_verified() && is_on(&controls.format_enabled),
            ),
        )?;

        // Finishing the wizard on a provider is also how a user leaves the
        // local backend; without this the rows below would be saved and
        // ignored, because the engine would still be transcribing on-device.
        settings.set("transcription_backend", "remote")?;
        settings.set("provider", &join_provider(&kind, &url))?;
        settings.set("api_key", key.trim())?;
        settings.set(
            "same_provider",
            // Identical verified endpoints must use the tested primary key,
            // not an older, separately stored formatting key.
            bool_setting(is_on(&controls.same_provider) || self.cleanup_is_verified()),
        )?;
        // Leave a separate provider's saved endpoint and credentials intact.
        // This wizard cannot verify its key; configure it in Settings instead.
        if self.cleanup_is_verified() {
            settings.set(
                "formatting_provider",
                &join_provider(&self.selected_cleanup_provider(), &url),
            )?;
        }
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

    /// Save the private backend, request protection and user preferences.
    ///
    /// No provider row is written, deliberately. The user has not chosen an
    /// online provider, and inventing one would put a service they never picked
    /// in front of any later switch back to online transcription. Setup still
    /// counts as configured, not installed: the closing panel explicitly hands
    /// off to the runner and model installation controls.
    fn save_local(&self) -> Result<(), String> {
        let ivars = self.ivars();
        let settings = ivars.engine.settings();
        // Arm the actual request guard before selecting the backend. Local
        // speech alone must not leave a previously configured cloud cleanup on.
        settings.set("local_only", "true")?;
        settings.set(
            "format_enabled",
            cleanup_setting(true, is_on(&ivars.controls.format_enabled)),
        )?;
        settings.set("transcription_backend", "local")?;
        let controls = &ivars.controls;
        settings.set("save_history", bool_setting(is_on(&controls.save_history)))?;
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
        let destination = finish_destination(is_local_card(&self.selected_provider()));
        self.dismiss();
        crate::app::with_app(|app| {
            app.with_settings(|page| page.reload());
            app.show_main(Some(destination));
        });
    }

    /// Every exit must restore the global shortcut and remove its temporary
    /// recorder monitor, even Escape while capturing a new shortcut.
    fn dismiss(&self) {
        self.stop_recording_hotkey();
        self.invalidate_connection();
        self.ivars().window.makeFirstResponder(None);
        crate::ui::dismiss_sheet(&self.ivars().window, "onboarding");
    }

    // ── Connection test ───────────────────────────────────

    fn request_models(&self) {
        let ivars = self.ivars();
        let (kind, url, key) = self.provider_fields();
        let provider = join_provider(&kind, &url);
        if !connection_allowed(ivars.engine.settings().local_only(), &provider) {
            self.set_text(&ivars.controls.connection_status, CLOUD_BLOCKED_MESSAGE);
            return;
        }
        if let Err(error) = validate_provider(&kind, &url, &key) {
            self.set_text(&ivars.controls.connection_status, &error);
            return;
        }
        self.set_text(&ivars.controls.connection_status, "Checking access...");
        let generation = ivars.connection_generation.get().wrapping_add(1);
        ivars.connection_generation.set(generation);
        let engine = Arc::clone(&ivars.engine);
        let key = key.trim().to_string();
        let key = (!key.is_empty()).then_some(key);
        crate::app::spawn(async move {
            let result = engine.fetch_models(Some(provider), key).await;
            crate::events::on_main(move || {
                crate::app::with_app(|app| {
                    app.with_onboarding(|window| {
                        if window.ivars().connection_generation.get() == generation {
                            window.models_loaded(&result);
                        }
                    })
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
                        "Connected to {}. Access checked; {} models listed. Speech support depends on your selected model.",
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
        ivars
            .connection_generation
            .set(ivars.connection_generation.get().wrapping_add(1));
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

fn name_switch(switch: &NSSwitch, name: &str) {
    // SAFETY: NSSwitch is an accessibility element, created and updated on
    // AppKit's main thread; AppKit copies the supplied NSString label.
    unsafe {
        let _: () = msg_send![switch, setAccessibilityLabel: &*NSString::from_str(name)];
    }
}

fn bool_setting(on: bool) -> &'static str {
    if on {
        "true"
    } else {
        "false"
    }
}

fn should_replace_default(model: &str, kind: &str) -> bool {
    model.trim().is_empty()
        || PROVIDER_OPTIONS.iter().any(|option| {
            model.trim()
                == if kind == "stt" {
                    option.stt_default
                } else {
                    option.chat_default
                }
        })
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

/// Hide cloud-specific fields and close their two-row gap without discarding
/// their values. Returning to cloud setup restores exactly the same layout.
fn configure_private_preferences(controls: &Controls, local: bool) {
    if controls.stt_model.isHidden() != local {
        let delta = (controls.stt_model.frame().origin.y - controls.chat_model.frame().origin.y)
            * 2.0
            + controls.cleanup_note.frame().size.height
            + crate::ui::GAP;
        // SAFETY: these retained AppKit controls and their parent are created
        // and accessed exclusively on the main thread; no view is removed here.
        if let Some(view) = unsafe { controls.stt_model.superview() } {
            let fixed: Vec<&NSView> = vec![
                &controls.stt_model,
                &controls.chat_model,
                &controls.preferences_intro,
                &controls.model_labels[0],
                &controls.model_labels[1],
                &controls.format_enabled,
                &controls.cleanup_note,
            ];
            for child in view.subviews() {
                if !fixed.iter().any(|item| std::ptr::eq(*item, &*child)) {
                    let mut frame = child.frame();
                    frame.origin.y += if local { delta } else { -delta };
                    child.setFrame(frame);
                }
            }
        }
    }
    controls.stt_model.setHidden(local);
    controls.chat_model.setHidden(local);
    controls.format_enabled.setHidden(local);
    controls.cleanup_note.setHidden(local);
    for field in &controls.model_labels {
        field.setHidden(local);
    }
}

fn configure_done(controls: &Controls, local: bool) {
    let mut frame = controls.summary.frame();
    let top = frame.origin.y + frame.size.height;
    frame.size.height = if local { 230.0 } else { 120.0 };
    frame.origin.y = top - frame.size.height;
    controls.summary.setFrame(frame);
    for field in &controls.done_support {
        field.setHidden(local);
    }
}

// ── Panel construction ────────────────────────────────────

fn build_panels(mtm: MainThreadMarker) -> (Retained<NSTabView>, Controls) {
    let panels = NSTabView::initWithFrame(
        NSTabView::alloc(mtm),
        NSRect::new(
            NSPoint::new(24.0, 84.0),
            NSSize::new(STEP_WIDTH, STEP_HEIGHT),
        ),
    );
    // A wizard, not a tab bar: the header and the buttons are the navigation.
    panels.setTabViewType(NSTabViewType::NoTabsNoBorder);

    let welcome_view = build_welcome(mtm);
    let (provider_view, providers, local_card, same_provider, formatting_provider) =
        build_provider(mtm);
    let (credentials_view, provider_url, api_key, connection_status, test) = build_credentials(mtm);
    let (
        preferences_view,
        stt_model,
        chat_model,
        microphone,
        microphone_note,
        refresh,
        hotkey,
        save_history,
        preferences_intro,
        model_labels,
        format_enabled,
        cleanup_note,
    ) = build_preferences(mtm);
    let (done_view, summary, done_support) = build_done(mtm);

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
            NSPoint::new(24.0, WINDOW_HEIGHT - 32.0),
            NSSize::new(STEP_WIDTH, 14.0),
        ),
    );
    kicker.setFont(Some(&crate::ui::body_font(11.0)));
    let heading = NSTextField::labelWithString(&NSString::from_str(""), mtm);
    heading.setFrame(NSRect::new(
        NSPoint::new(24.0, WINDOW_HEIGHT - 74.0),
        NSSize::new(STEP_WIDTH, 36.0),
    ));
    heading.setFont(Some(&crate::ui::display_font(28.0)));

    let error = note(
        mtm,
        "",
        NSRect::new(NSPoint::new(24.0, 52.0), NSSize::new(STEP_WIDTH, 28.0)),
    );
    error.setTextColor(Some(&objc2_app_kit::NSColor::systemRedColor()));
    allow_wrapping(&error, STEP_WIDTH);

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
        microphone_note,
        microphone_ids: RefCell::new(vec![String::new()]),
        refresh,
        hotkey,
        save_history,
        preferences_intro,
        model_labels,
        format_enabled,
        cleanup_note,
        done_support,
        summary,
    };
    (panels, controls)
}

fn build_welcome(mtm: MainThreadMarker) -> Retained<NSView> {
    let mut form = Form::new(mtm, STEP_WIDTH, STEP_HEIGHT);
    add_paragraph(&mut form, mtm,
        "Turn a thought into text, without leaving what you’re doing. A short setup puts you in control of how it works.", 54.0);
    for (title, body) in [
        ("01   Choose your comfort zone", "Keep speech on this Mac, or connect a cloud provider. You can change this later."),
        ("02   Make it feel natural", "Pick a microphone and a shortcut. Hold to speak; release to turn your words into text."),
        ("03   Start where you work", "Use OpenFlow in messages, documents and other apps. Review or copy your latest result in the workspace."),
    ] {
        let frame = form.full(26.0);
        let title = label(mtm, title, frame);
        title.setFont(Some(&crate::ui::display_font(17.0)));
        title.setAlignment(objc2_app_kit::NSTextAlignment::Left);
        form.add(&title);
        add_paragraph(&mut form, mtm, body, 42.0);
        form.full(6.0);
    }
    add_paragraph(&mut form, mtm,
        "No microphone is started during setup. Nothing is saved until you finish, except a shortcut you choose to record.", 42.0);
    form.view.clone()
}

fn add_paragraph(
    form: &mut Form,
    mtm: MainThreadMarker,
    text: &str,
    height: f64,
) -> Retained<NSTextField> {
    let frame = form.full(height);
    let field = note(mtm, text, frame);
    field.setFont(Some(&crate::ui::body_font(13.0)));
    allow_wrapping(&field, frame.size.width);
    form.add(&field);
    field
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
    add_paragraph(&mut form, mtm,
        "Choose private, on-device speech or a cloud service. Cloud dictation sends audio to your provider; optional cleanup sends transcript text.", 40.0);

    let frame = form.full(24.0);
    let local_card = radio(mtm, frame, LOCAL_CARD_LABEL, TAG_LOCAL_CARD);
    local_card.setFont(Some(&crate::ui::display_font(15.0)));
    form.add(&local_card);
    add_paragraph(&mut form, mtm, LOCAL_CARD_DESCRIPTION, 36.0);

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
        button.setFont(Some(&crate::ui::body_font(13.0)));
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

    let (l, c) = form.row(ROW);
    form.add(&label(mtm, "Same for cleanup", l));
    let same_provider = switch_control(
        mtm,
        NSRect::new(c.origin, NSSize::new(38.0, c.size.height)),
        TAG_SAME_PROVIDER,
    );
    name_switch(&same_provider, "Use transcription provider for cleanup");
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
        "Cleanup is optional text rewriting. Cloud cleanup is disabled for private setup.",
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
    add_paragraph(&mut form, mtm,
        "An API key is a credential that lets OpenFlow use your provider account. Create one in the provider’s dashboard, then paste it below. Provider usage may require payment or credits.", 64.0);

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
    connection_status.setFont(Some(&crate::ui::body_font(13.0)));
    allow_wrapping(&connection_status, STEP_WIDTH);
    form.add(&connection_status);
    add_paragraph(&mut form, mtm,
        "Your key is stored in macOS Keychain. Audio is sent only when you dictate. A successful connection checks access, not the accuracy of a speech model.", 54.0);

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
    Retained<NSTextField>,
    Retained<NSButton>,
    Retained<NSButton>,
    Retained<NSSwitch>,
    Retained<NSTextField>,
    Vec<Retained<NSTextField>>,
    Retained<NSSwitch>,
    Retained<NSTextField>,
) {
    let mut form = Form::new(mtm, STEP_WIDTH, STEP_HEIGHT);
    let frame = form.full(42.0);
    let preferences_intro = note(
        mtm,
        "Smart defaults are ready. Change a model now or paste any compatible model id later.",
        frame,
    );
    preferences_intro.setFont(Some(&crate::ui::body_font(13.0)));
    allow_wrapping(&preferences_intro, STEP_WIDTH);
    form.add(&preferences_intro);

    let (l, c) = form.row(ROW);
    let stt_label = label(mtm, "Speech-to-text model", l);
    form.add(&stt_label);
    let stt_model = combo(mtm, c, 0);
    form.add(&stt_model);

    let (l, c) = form.row(ROW);
    let chat_label = label(mtm, "Clean up wording", l);
    form.add(&chat_label);
    let format_enabled = switch_control(
        mtm,
        NSRect::new(c.origin, NSSize::new(38.0, c.size.height)),
        0,
    );
    name_switch(&format_enabled, "Clean up wording");
    form.add(&format_enabled);
    let chat_model = combo(
        mtm,
        NSRect::new(
            NSPoint::new(c.origin.x + 48.0, c.origin.y),
            NSSize::new(c.size.width - 48.0, c.size.height),
        ),
        0,
    );
    form.add(&chat_model);
    let cleanup_note = add_paragraph(&mut form, mtm, CLEANUP_DISCLOSURE, 26.0);

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
    let n = form.control_only(MICROPHONE_NOTE_HEIGHT);
    let microphone_note = note(mtm, "", n);
    allow_wrapping(&microphone_note, n.size.width);
    form.add(&microphone_note);

    let (l, c) = form.row(ROW);
    form.add(&label(mtm, "Record shortcut", l));
    let hotkey = button(mtm, c, "Option+V", 0);
    form.add(&hotkey);
    form.note_row(
        mtm,
        "Click, then press the chord. Hold it to record, release it to transcribe.",
    );

    let (l, c) = form.row(ROW);
    form.add(&label(mtm, "Save dictation history", l));
    let save_history = switch_control(
        mtm,
        NSRect::new(c.origin, NSSize::new(38.0, c.size.height)),
        0,
    );
    set_switch(&save_history, true);
    name_switch(&save_history, "Save dictation history");
    form.add(&save_history);
    add_paragraph(&mut form, mtm,
        "History is optional and stored locally, without encryption. Turn it off to avoid keeping future dictations. Existing history is not deleted.", 36.0);

    (
        form.view.clone(),
        stt_model,
        chat_model,
        microphone,
        microphone_note,
        refresh,
        hotkey,
        save_history,
        preferences_intro,
        vec![stt_label, chat_label],
        format_enabled,
        cleanup_note,
    )
}

fn build_done(
    mtm: MainThreadMarker,
) -> (
    Retained<NSView>,
    Retained<NSTextField>,
    Vec<Retained<NSTextField>>,
) {
    let mut form = Form::new(mtm, STEP_WIDTH, STEP_HEIGHT);
    let frame = form.full(28.0);
    form.add(&note(
        mtm,
        "Setup is saved. Here is what OpenFlow will use:",
        frame,
    ));
    let frame = form.full(120.0);
    let summary = note(mtm, "", frame);
    summary.setFont(Some(&crate::ui::body_font(14.0)));
    allow_wrapping(&summary, STEP_WIDTH);
    form.add(&summary);
    let workspace_help = add_paragraph(&mut form, mtm,
        "Try a short phrase in your workspace first. macOS may ask for Microphone and Accessibility access. You can always copy a result if automatic insertion is unavailable.", 54.0);
    let settings_help = add_paragraph(&mut form, mtm,
        "OpenFlow stays in the menu bar. Settings keeps your processing, privacy and shortcut choices in one place. Only enable plugins you trust; Local-only does not sandbox plugin code.", 58.0);
    (
        form.view.clone(),
        summary,
        vec![workspace_help, settings_help],
    )
}

/// Actual views with synthetic display values only: never constructs an Engine,
/// opens Keychain, enumerates microphones, or starts a connection/recording.
pub(super) fn preview_views(mtm: MainThreadMarker) -> Vec<(String, Retained<NSView>)> {
    let mut scenarios: Vec<_> = Step::ORDER
        .into_iter()
        .map(|step| (step, false, "", format!("onboarding-{}", step.index() + 1)))
        .collect();
    scenarios.extend([
        (
            Step::Preferences,
            true,
            "",
            "onboarding-private-preferences".to_string(),
        ),
        (Step::Done, true, "", "onboarding-private-done".to_string()),
        (
            Step::Preferences,
            false,
            "cleanup-on",
            "onboarding-cleanup-enabled".to_string(),
        ),
        (
            Step::Preferences,
            false,
            "cleanup-off",
            "onboarding-cleanup-disabled".to_string(),
        ),
        (
            Step::Preferences,
            false,
            "cleanup-separate",
            "onboarding-cleanup-needs-setup".to_string(),
        ),
        (
            Step::Credentials,
            false,
            "checking",
            "onboarding-connection-checking".to_string(),
        ),
        (
            Step::Credentials,
            false,
            "error",
            "onboarding-connection-error".to_string(),
        ),
    ]);
    scenarios.into_iter().map(|(step, local, state, name)| {
        let (panels, controls) = build_panels(mtm);
        panels.selectTabViewItemAtIndex(step.index() as isize);
        controls.kicker.setStringValue(&NSString::from_str(&progress_text(step, local)));
        controls.heading.setStringValue(&NSString::from_str(step.title()));
        controls.primary.setTitle(&NSString::from_str(step.primary_title()));
        controls.back.setHidden(step == Step::Welcome || step == Step::Done);
        controls.later.setHidden(step == Step::Done);
        controls.providers[0].setState(NSControlStateValueOn);
        set_switch(&controls.format_enabled, state != "cleanup-off");
        controls.stt_model.setStringValue(&NSString::from_str("whisper-large-v3-turbo"));
        controls.chat_model.setStringValue(&NSString::from_str("openai/gpt-oss-20b"));
        controls.microphone.addItemWithTitle(&NSString::from_str("System default"));
        controls.microphone_note.setStringValue(&NSString::from_str("Microphone permission is checked when recording starts."));
        controls.preferences_intro.setStringValue(&NSString::from_str("Defaults are filled in for you. Microphone permission lets OpenFlow hear you; Accessibility permission lets it insert text into other apps."));
        controls.summary.setStringValue(&NSString::from_str(&summary_line("groq", "whisper-large-v3-turbo", "System default", "Option+V")));
        if local {
            configure_private_preferences(&controls, true);
            configure_done(&controls, true);
            controls.preferences_intro.setStringValue(&NSString::from_str(PRIVATE_INTRO));
            controls.summary.setStringValue(&NSString::from_str(PRIVATE_SUMMARY));
            if step == Step::Done {
                controls.heading.setStringValue(&NSString::from_str("Next: install your private engine"));
                controls.primary.setTitle(&NSString::from_str("Continue to local setup"));
            }
        }
        if state == "checking" {
            controls.connection_status.setStringValue(&NSString::from_str("Checking access…"));
        } else if state == "error" {
            controls.connection_status.setStringValue(&NSString::from_str("Connection could not be verified. Check that your key is active and your provider account has access, then test again."));
            controls.error.setStringValue(&NSString::from_str("Test the connection before continuing."));
        } else if state == "cleanup-separate" {
            set_switch(&controls.format_enabled, false);
            controls.format_enabled.setEnabled(false);
            controls.chat_model.setEnabled(false);
            controls.cleanup_note.setStringValue(&NSString::from_str(SEPARATE_CLEANUP_DISCLOSURE));
        }
        let view = NSView::initWithFrame(NSView::alloc(mtm), NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WINDOW_WIDTH, WINDOW_HEIGHT)));
        view.addSubview(&panels);
        for child in [&*controls.kicker as &NSView, &controls.heading, &controls.error, &controls.back, &controls.later, &controls.primary] {
            view.addSubview(child);
        }
        (name, view)
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_setup_reaches_preferences_without_credentials_and_can_go_back() {
        assert_eq!(next_step(Step::Provider, true), Step::Preferences);
        assert_eq!(previous_step(Step::Preferences, true), Step::Provider);
        assert_eq!(next_step(Step::Preferences, true), Step::Done);
        assert_eq!(next_step(Step::Provider, false), Step::Credentials);
        assert_eq!(previous_step(Step::Preferences, false), Step::Credentials);
        assert_eq!(
            progress_text(Step::Preferences, true),
            "SET UP OPENFLOW  ·  3 OF 4"
        );
        assert_eq!(
            progress_text(Step::Done, false),
            "SET UP OPENFLOW  ·  5 OF 5"
        );
    }

    #[test]
    fn private_copy_discloses_initial_download_and_never_promises_ready() {
        assert!(LOCAL_CARD_DESCRIPTION.contains("one-time model download"));
        assert!(LOCAL_CARD_DESCRIPTION.contains("offline after setup"));
        assert!(!Step::Done.title().contains("ready"));
        assert!(PRIVATE_SUMMARY.contains("plugins are separate programs"));
        assert!(PRIVATE_SUMMARY.contains("not confined by Local-only"));
        assert_eq!(finish_destination(true), "providers");
        assert!(crate::ui::settings::section_index(finish_destination(true)).is_some());
        assert!(summary_line("groq", "", "System default", "Option+V")
            .contains("permission may still be needed"));
    }

    #[test]
    fn cleanup_consent_is_respected_and_private_setup_forces_it_off() {
        assert_eq!(cleanup_setting(false, true), "true");
        assert_eq!(cleanup_setting(false, false), "false");
        assert_eq!(cleanup_setting(true, true), "false");
        assert_eq!(cleanup_setting(true, false), "false");
        let source = include_str!("onboarding.rs");
        let implementation = source.split("#[cfg(test)]").next().unwrap();
        assert!(
            implementation.contains("settings.format_enabled()"),
            "reload respects saved consent"
        );
        assert!(
            implementation
                .contains("self.cleanup_is_verified() && is_on(&controls.format_enabled)"),
            "cloud save independently guards the visible choice"
        );
    }

    #[test]
    fn cleanup_requires_the_verified_speech_endpoint_and_capability() {
        for (stt, cleanup) in [
            ("deepgram", "groq"),
            ("deepgram", "deepgram"),
            ("groq", "openai"),
        ] {
            assert!(!cleanup_verified(stt, "", cleanup, "", true));
        }
        assert!(cleanup_verified("groq", "", "groq", "", true));
        assert!(!cleanup_verified("groq", "", "groq", "", false));
        assert!(cleanup_verified(
            "custom",
            "http://localhost:8080/v1",
            "custom",
            "http://localhost:8080/v1/",
            true
        ));
        assert!(!cleanup_verified(
            "custom",
            "http://localhost:8080/v1",
            "custom",
            "http://localhost:8081/v1",
            true
        ));
        assert!(!cleanup_verified("local", "", "groq", "", true));
        for requested in [false, true] {
            assert_eq!(
                cleanup_setting(
                    false,
                    requested && cleanup_verified("deepgram", "", "groq", "", true)
                ),
                "false"
            );
        }
    }

    #[test]
    fn privacy_switches_have_explicit_accessibility_names() {
        let source = include_str!("onboarding.rs");
        let implementation = source.split("#[cfg(test)]").next().unwrap();
        for expected in [
            "name_switch(&format_enabled, \"Clean up wording\")",
            "name_switch(&save_history, \"Save dictation history\")",
            "name_switch(&same_provider, \"Use transcription provider for cleanup\")",
        ] {
            assert!(
                implementation.contains(expected),
                "missing AX name: {expected}"
            );
        }
        assert!(implementation.contains("setAccessibilityLabel:"));
    }

    #[test]
    fn connection_checks_respect_local_only_without_blocking_this_mac() {
        for provider in [
            "custom:http://localhost:8123/v1",
            "custom:http://127.0.0.1:8123/v1",
            "custom:http://[::1]:8123/v1",
        ] {
            assert!(connection_allowed(true, provider), "{provider}");
            assert!(connection_allowed(false, provider), "{provider}");
        }
        for provider in [
            "openrouter",
            "groq",
            "custom:https://api.example.com/v1",
            "custom:http://192.168.1.10:8123/v1",
            "custom:http://localhost.example.com/v1",
            "custom:not-a-url",
            "",
        ] {
            assert!(!connection_allowed(true, provider), "{provider}");
            assert!(connection_allowed(false, provider), "{provider}");
        }
    }

    #[test]
    fn local_only_recovery_points_to_the_section_owning_the_control() {
        assert!(CLOUD_BLOCKED_MESSAGE.contains("Settings → Providers"));
        assert!(CLOUD_BLOCKED_MESSAGE.contains("select On this Mac"));
        assert_eq!(finish_destination(true), "providers");
        let source = include_str!("settings.rs");
        let local_panel = source
            .split("fn build_local")
            .nth(1)
            .expect("local settings panel exists");
        assert!(
            local_panel
                .split("\nfn ")
                .next()
                .unwrap()
                .contains("TAG_LOCAL_ONLY"),
            "recovery target must contain the actual privacy control"
        );
    }

    #[test]
    fn changing_providers_updates_shipped_defaults_but_preserves_custom_models() {
        assert!(should_replace_default("whisper-large-v3-turbo", "stt"));
        assert!(should_replace_default("gpt-4o-mini", "chat"));
        assert!(should_replace_default("  ", "stt"));
        assert!(!should_replace_default("my-custom-speech-model", "stt"));
        assert!(!should_replace_default("my-custom-cleanup-model", "chat"));
    }

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
        assert_eq!(
            progress_text(Step::Welcome, false),
            "SET UP OPENFLOW  ·  1 OF 5"
        );
        assert_eq!(
            progress_text(Step::Done, false),
            "SET UP OPENFLOW  ·  5 OF 5"
        );
        assert_eq!(
            progress_text(Step::Done, true),
            "SET UP OPENFLOW  ·  4 OF 4"
        );
    }

    #[test]
    fn all_sheet_exit_paths_restore_shortcut_recording() {
        let source = include_str!("onboarding.rs");
        let implementation = source.split("#[cfg(test)]").next().unwrap();
        assert_eq!(
            implementation.matches("crate::ui::dismiss_sheet(").count(),
            1,
            "all exits must use the centralized recorder cleanup"
        );
        let dismiss = implementation
            .split("fn dismiss(&self)")
            .nth(1)
            .unwrap()
            .split("\n    }")
            .next()
            .unwrap();
        assert!(
            dismiss.find("self.stop_recording_hotkey()").unwrap()
                < dismiss.find("crate::ui::dismiss_sheet(").unwrap()
        );
        for handler in [
            "fn skip_setup(",
            "fn window_should_close(",
            "fn finish(&self)",
        ] {
            let body = implementation
                .split(handler)
                .nth(1)
                .unwrap()
                .split("\n    }")
                .next()
                .unwrap();
            assert!(
                body.contains("self.dismiss()"),
                "{handler} bypasses cleanup"
            );
        }
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
            "Groq · whisper-large-v3 · MacBook Pro Microphone · shortcut: Option+V. Microphone and Accessibility permission may still be needed."
        );
        assert_eq!(
            summary_line("groq", "  ", "System default", "Option+V"),
            "Groq · whisper-large-v3-turbo · System default · shortcut: Option+V. Microphone and Accessibility permission may still be needed."
        );
    }

    fn device(name: &str, is_default: bool) -> AudioDevice {
        AudioDevice {
            id: format!("{name}::1"),
            name: name.to_string(),
            is_default,
        }
    }

    /// The wizard used to add "System default" whether or not anything had been
    /// enumerated, so a machine with no usable input looked exactly like a
    /// machine with one. Both ways of finding nothing -- `Err` from a wedged
    /// audio thread, `Ok(vec![])` from a failed enumeration or a denied grant --
    /// arrive here as an empty slice, and neither may produce an item naming a
    /// device that does not exist.
    #[test]
    fn an_empty_device_list_does_not_invent_a_microphone() {
        assert_eq!(microphone_items(&[]), vec![NO_MICROPHONE.to_string()]);
        assert!(
            !microphone_items(&[]).contains(&"System default".to_string()),
            "nothing enumerated means there is no default to fall back on either"
        );

        // With devices the list is unchanged: the fallback first, then every
        // device, so the popup stays aligned with the saved id list.
        assert_eq!(
            microphone_items(&[
                device("MacBook Pro Microphone", true),
                device("Yeti", false)
            ]),
            vec![
                "System default".to_string(),
                "MacBook Pro Microphone (default)".to_string(),
                "Yeti".to_string(),
            ]
        );
    }

    /// The readiness line under the picker keeps the two failures apart, since
    /// the user's next move differs, and never reports a count it did not get.
    #[test]
    fn the_microphone_note_says_which_kind_of_nothing_it_found() {
        assert_eq!(
            microphone_note(Ok(0)),
            "No microphone detected yet. Check system permission, then press Refresh."
        );
        assert_eq!(microphone_note(Ok(1)), "1 microphone ready.");
        assert_eq!(microphone_note(Ok(3)), "3 microphones ready.");
        // The audio thread's own words, as `list_audio_devices` hands them over.
        assert_eq!(
            microphone_note(Err("Device list timeout")),
            "Could not read microphones: Device list timeout. Press Refresh to try again."
        );
    }

    /// The closing panel is the last thing setup says, and it may not say the
    /// app is ready to dictate when nothing can hear the user. Everything it
    /// promises about a machine that does have a microphone is unchanged.
    #[test]
    fn the_closing_panel_does_not_claim_a_microphone_it_never_found() {
        let missing = &microphone_items(&[])[0];

        assert_eq!(done_heading(missing), "Saved. Connect a microphone next.");
        assert_eq!(
            summary_line("groq", "whisper-large-v3", missing, "Option+V"),
            "Groq · whisper-large-v3 · No microphone detected · connect an input \
device, then hold Option+V to dictate"
        );

        // A real device, or the fallback that only exists when one was found,
        // still gets the ready wording.
        for microphone in microphone_items(&[device("MacBook Pro Microphone", true)]) {
            assert_eq!(done_heading(&microphone), "Your preferences are saved");
            assert!(
                summary_line("groq", "whisper-large-v3", &microphone, "Option+V")
                    .ends_with("permission may still be needed."),
                "{microphone} must keep the ready wording"
            );
        }
    }
    /// The id list both popups keep is positional: index 0 is the empty
    /// string, then one id per device, and `write` turns the selected index
    /// straight back into an id. So `microphone_items` is not free to change
    /// how many rows it returns -- one extra or one fewer and the selection
    /// silently saves a different microphone than the one on screen, or the
    /// empty string.
    ///
    /// This became worth holding when Settings' `reload_microphones` became
    /// the second caller: it builds the same id list, so a change made for one
    /// popup now moves the other.
    #[test]
    fn the_item_list_stays_the_same_length_as_the_id_list() {
        for count in 0..4 {
            let devices: Vec<AudioDevice> = (0..count)
                .map(|n| device(&format!("Microphone {n}"), n == 0))
                .collect();

            // Exactly what both callers build alongside the items.
            let mut ids = vec![String::new()];
            ids.extend(devices.iter().map(|d| d.id.clone()));

            let items = microphone_items(&devices);
            assert_eq!(
                items.len(),
                ids.len(),
                "{count} device(s): the popup shows {items:?} against ids {ids:?}, \
so a selection maps to the wrong device"
            );
            for (index, id) in ids.iter().enumerate().skip(1) {
                assert!(
                    items[index].starts_with(&devices[index - 1].name),
                    "row {index} shows {:?} but would save {id:?}",
                    items[index]
                );
            }
        }
    }

    /// The height of one line of [`note`] text at the size `note` sets, which
    /// is what [`MICROPHONE_NOTE_HEIGHT`] is a multiple of. Pinned by
    /// `one_line_of_note_text_is_one_note_line` rather than assumed.
    const NOTE_LINE: f64 = 13.0;

    /// The column the microphone line is laid out in.
    ///
    /// `Form::control_only` hands back the form's width less the label column,
    /// and `build_preferences` builds its form at [`STEP_WIDTH`], so this is
    /// the same arithmetic the panel does rather than a number copied off it.
    /// The wizard is a fixed-size sheet, so there is only the one width.
    fn microphone_note_width() -> f64 {
        STEP_WIDTH - crate::ui::CONTROL_X
    }

    /// How many lines `text` takes when wrapped into a column `width` wide, at
    /// the font [`note`] uses.
    ///
    /// AppKit measures text without a main thread and without an
    /// `NSApplication`, so the one layout question the wizard cannot be asked
    /// -- step 3 is behind a connection test no test run can pass, and nothing
    /// short of a real provider key gets a person to this panel -- is
    /// arithmetic instead. Measured through `NSAttributedString`, which agrees
    /// to the point with what `wrap` gets from a real `NSTextField`.
    fn wrapped_lines(text: &str, width: f64) -> usize {
        use objc2::runtime::AnyObject;
        use objc2_app_kit::{
            NSAttributedStringNSExtendedStringDrawing, NSFont, NSFontAttributeName,
            NSStringDrawingOptions,
        };
        use objc2_foundation::{NSAttributedString, NSDictionary};

        let font = NSFont::systemFontOfSize(10.0);
        let font: &AnyObject = &font;
        let attributes = NSDictionary::from_slices(&[unsafe { NSFontAttributeName }], &[font]);
        let string = NSString::from_str(text);
        let attributed = unsafe { NSAttributedString::new_with_attributes(&string, &attributes) };
        let height = attributed
            .boundingRectWithSize_options_context(
                NSSize::new(width, f64::MAX),
                NSStringDrawingOptions::UsesLineFragmentOrigin,
                None,
            )
            .size
            .height;
        (height / NOTE_LINE).round() as usize
    }

    /// The words the audio thread puts inside the error wording, read out of
    /// `openflow-core` rather than copied here.
    ///
    /// [`microphone_note`] quotes them verbatim -- `list_audio_devices` is
    /// `Recorder::list_devices` and nothing in between rewrites them -- so a
    /// longer one written over there is exactly how this box overflows, and a
    /// copy kept here would go on passing while it did.
    fn audio_thread_errors() -> Vec<&'static str> {
        const SOURCE: &str = include_str!("../../../openflow-core/src/audio.rs");
        let at = SOURCE
            .find("pub fn list_devices")
            .expect("openflow-core still enumerates devices through Recorder::list_devices");
        let body = &SOURCE[at..];
        let body = &body[..body.find("\n    pub fn ").unwrap_or(body.len())];

        let mut errors = Vec::new();
        let mut rest = body;
        while let Some(open) = rest.find('"') {
            let after = &rest[open + 1..];
            let Some(close) = after.find('"') else { break };
            let (literal, tail) = after.split_at(close);
            let tail = &tail[1..];
            if tail.starts_with(".to_string()") {
                errors.push(literal);
            }
            rest = tail;
        }
        assert_eq!(
            errors.len(),
            2,
            "the scan found {errors:?}, so it has stopped matching how \
Recorder::list_devices words its failures"
        );
        errors
    }

    /// [`NOTE_LINE`] has to be what a line of note text actually measures, or
    /// every line count below is off by whatever it is wrong by.
    #[test]
    fn one_line_of_note_text_is_one_note_line() {
        assert_eq!(
            wrapped_lines("Ag", microphone_note_width()),
            1,
            "a short note is one line, or NOTE_LINE disagrees with the font"
        );
    }

    /// The line under the microphone picker has to fit the box reserved for
    /// it, and this is the only way anyone finds out.
    ///
    /// Step 3 of the wizard is behind `can_advance`, which will not open the
    /// Credentials panel without a connection a test run cannot make, so this
    /// panel has never been on screen outside a hand-driven session with a
    /// real key. The reservation was arithmetic done by hand; here it is done
    /// by AppKit, against the strings [`microphone_note`] actually returns and
    /// the height [`build_preferences`] actually reserves. Nothing clips this
    /// label, so a third line is not an ellipsis -- it draws over the Record
    /// shortcut row below it.
    #[test]
    fn every_microphone_note_fits_the_line_reserved_for_it() {
        let width = microphone_note_width();
        let reserved = (MICROPHONE_NOTE_HEIGHT / NOTE_LINE).floor() as usize;
        assert!(
            reserved >= 1,
            "{MICROPHONE_NOTE_HEIGHT}pt does not hold a single {NOTE_LINE}pt line"
        );

        // Every arm of the match, and enough counts to cover the singular, the
        // plural, and a machine with an unlikely number of inputs.
        let mut messages: Vec<String> = (0..=16).map(|count| microphone_note(Ok(count))).collect();
        messages.extend(
            audio_thread_errors()
                .into_iter()
                .map(|error| microphone_note(Err(error))),
        );

        for message in messages {
            let lines = wrapped_lines(&message, width);
            assert!(
                lines <= reserved,
                "{lines} lines in {width}pt, but the picker's note reserves \
{MICROPHONE_NOTE_HEIGHT}pt, which is {reserved}: {message:?}"
            );
        }
    }
}
