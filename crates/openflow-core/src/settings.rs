//! Typed access to the settings table, and the one place that decides which
//! keys are secrets.
//!
//! Every default here matches what the settings UI shows for an unset key, so a
//! fresh install behaves the same whether the user has opened Settings or not.

use crate::db::Database;
use crate::hotkey::{self, HotKey};
use crate::insert::{ClipboardPolicy, InsertMethod};
use crate::secrets::SecretStore;
use crate::transcribe::Provider;

/// Keys that live in the OS keychain and never in the settings table.
pub const SECRET_SETTINGS: &[&str] = &["api_key", "formatting_api_key", "tts_api_key"];

pub fn is_secret_setting(key: &str) -> bool {
    SECRET_SETTINGS.contains(&key)
}

/// The transcription provider a fresh install uses.
pub const DEFAULT_PROVIDER: &str = "groq";
/// The voice provider a fresh install uses.
pub const DEFAULT_TTS_PROVIDER: &str = "openrouter";
/// Where the overlay pill sits until the user drags it somewhere else.
pub const DEFAULT_OVERLAY_POSITION: &str = "left-center";
/// The audio container speech is requested in when nothing else is chosen.
pub const DEFAULT_TTS_RESPONSE_FORMAT: &str = "mp3";
/// The local model a fresh install would use: the one that keeps proper nouns.
pub const DEFAULT_LOCAL_MODEL: &str = "accurate";
/// How long the local sidecar stays loaded with nothing to do.
pub const DEFAULT_LOCAL_IDLE_MINUTES: u64 = 10;

/// Where transcription happens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TranscriptionBackend {
    /// The configured provider, over the network.
    Remote,
    /// The supervised MLX sidecar on this Mac.
    Local,
}

impl TranscriptionBackend {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Remote => "remote",
            Self::Local => "local",
        }
    }
}

/// The settings table plus the keychain, behind one typed surface.
pub struct Settings {
    db: Database,
    secrets: SecretStore,
}

/// The live-preview decision, split out from the store so the rule can be read
/// and tested on its own. Anything that is not the literal "true" or "false"
/// falls back to the endpoint: a half-written setting must never be what starts
/// billing a hosted provider every 800 ms.
pub fn live_preview_allowed(setting: Option<&str>, provider: &Provider) -> bool {
    match setting {
        Some("true") => true,
        Some("false") => false,
        _ => provider.is_custom(),
    }
}

impl Settings {
    pub fn new(db: Database, secrets: SecretStore) -> Self {
        let settings = Self { db, secrets };
        // The network guard is a property of the process, so it is armed the
        // moment the stored value is readable rather than when the first
        // request happens to be made. Only a stored value speaks: an install
        // that has never touched the toggle leaves the guard at its default,
        // which is the same answer and does not have a second `Settings` in the
        // process (a test's scratch store, say) overwrite a live one.
        match settings.db.get_setting("local_only") {
            Ok(Some(stored)) => crate::transcribe::set_local_only(stored == "true"),
            Ok(None) => {}
            // A store that cannot be read has not said "off"; it has said
            // nothing, and the guard reads that the same way `local_only` does.
            Err(_) => crate::transcribe::set_local_only(true),
        }
        settings
    }

    pub fn db(&self) -> &Database {
        &self.db
    }

    pub fn secrets(&self) -> &SecretStore {
        &self.secrets
    }

    /// Read any key by name, routing the secret ones to the keychain.
    pub fn get(&self, key: &str) -> Result<Option<String>, String> {
        if is_secret_setting(key) {
            self.secrets.get(key)
        } else {
            self.db.get_setting(key)
        }
    }

    /// Write any key by name, routing the secret ones to the keychain.
    pub fn set(&self, key: &str, value: &str) -> Result<(), String> {
        let written = if is_secret_setting(key) {
            self.secrets.set(key, value)
        } else {
            self.db.set_setting(key, value)
        };
        // Turning Local only on has to bind the next request, not the next
        // launch, so the guard follows the write rather than being polled.
        if written.is_ok() && key == "local_only" {
            crate::transcribe::set_local_only(value == "true");
        }
        written
    }

    /// Lift any secret still sitting in the settings table into the keychain,
    /// and only then drop the plaintext copy. A failed write leaves the
    /// plaintext where it is rather than losing the user's key.
    pub fn migrate_secrets(&self) {
        for key in SECRET_SETTINGS {
            let Ok(Some(plaintext)) = self.db.get_setting(key) else {
                continue;
            };
            let secure_write_succeeded = match self.secrets.get(key) {
                Ok(Some(_)) => true,
                Ok(None) => match self.secrets.set(key, &plaintext) {
                    Ok(()) => true,
                    Err(error) => {
                        eprintln!("Could not migrate {} to secure storage: {}", key, error);
                        false
                    }
                },
                Err(error) => {
                    eprintln!("Could not inspect secure storage for {}: {}", key, error);
                    false
                }
            };
            if secure_write_succeeded {
                if let Err(error) = self.db.remove_setting(key) {
                    eprintln!("Could not remove migrated plaintext {}: {}", key, error);
                }
            }
        }
    }

    /// A key whose absence and whose unreadability mean the same thing: use
    /// the shipped default. Every setting where being wrong costs the user no
    /// more than a fresh install would reads through here, which is all of them
    /// except the two that protect the user.
    fn stored(&self, key: &str) -> Option<String> {
        self.db.get_setting(key).unwrap_or_default()
    }

    /// A toggle that is on until the user writes the literal "false".
    fn flag_on_by_default(&self, key: &str) -> bool {
        self.stored(key).map(|v| v != "false").unwrap_or(true)
    }

    /// A toggle that is off until the user writes the literal "true".
    fn flag_off_by_default(&self, key: &str) -> bool {
        self.stored(key).map(|v| v == "true").unwrap_or(false)
    }

    /// A text setting where blank is the same as unset.
    fn non_empty(&self, key: &str) -> Option<String> {
        self.stored(key).filter(|value| !value.trim().is_empty())
    }

    // ── Providers and models ──────────────────────────────
    pub fn provider_name(&self) -> String {
        self.stored("provider")
            .unwrap_or_else(|| DEFAULT_PROVIDER.to_string())
    }

    pub fn provider(&self) -> Provider {
        Provider::from_str(&self.provider_name())
    }

    /// The formatting endpoint as stored. `None` means "the same one we
    /// transcribe with", which is what the caller substitutes.
    pub fn formatting_provider_name(&self) -> Option<String> {
        self.stored("formatting_provider")
    }

    pub fn formatting_provider(&self) -> Provider {
        Provider::from_str(
            &self
                .formatting_provider_name()
                .unwrap_or_else(|| self.provider_name()),
        )
    }

    pub fn tts_provider_name(&self) -> String {
        self.stored("tts_provider")
            .unwrap_or_else(|| DEFAULT_TTS_PROVIDER.to_string())
    }

    pub fn tts_provider(&self) -> Provider {
        Provider::from_str(&self.tts_provider_name())
    }

    /// Whether one provider serves both transcription and cleanup.
    pub fn same_provider(&self) -> bool {
        self.flag_on_by_default("same_provider")
    }

    pub fn format_enabled(&self) -> bool {
        self.flag_on_by_default("format_enabled")
    }

    /// Whether a recording should be previewed as it is spoken.
    ///
    /// Unset means yes for a self-hosted endpoint, where a preview costs a LAN
    /// round trip, and no for a hosted one, where it bills for one every
    /// 800 ms. Set either way, the user's choice stands.
    ///
    /// Turning it on for a hosted provider has two consequences a Settings
    /// checkbox has to say out loud, because neither is visible from the pill:
    ///
    /// - **Rate limits.** A 20 s dictation makes up to 25 readings, which is
    ///   75 requests a minute against Groq's 20 RPM for audio. The final take is
    ///   the request that queues behind them, so the 429 lands on the
    ///   transcription the user is actually waiting for, not on a preview.
    /// - **Billing.** Each reading re-uploads the whole recording so far, and
    ///   hosted transcription bills per minute of audio. Previewing a 20 s take
    ///   bills roughly the sum of 0.8 s, 1.6 s ... 20 s -- about 4 minutes of
    ///   audio for 20 seconds of speech, on top of the take itself.
    pub fn live_preview(&self) -> bool {
        live_preview_allowed(self.stored("live_preview").as_deref(), &self.provider())
    }

    pub fn stt_model(&self) -> Option<String> {
        self.stored("stt_model")
    }

    pub fn chat_model(&self) -> Option<String> {
        self.stored("chat_model")
    }

    pub fn language(&self) -> Option<String> {
        self.stored("language")
    }

    /// Names and terms sent to the transcriber as a spelling hint.
    pub fn dictionary(&self) -> Option<String> {
        self.stored("dictionary")
    }

    // ── Secrets ───────────────────────────────────────────
    pub fn api_key(&self) -> Result<Option<String>, String> {
        self.secrets.get("api_key")
    }

    pub fn formatting_api_key(&self) -> Result<Option<String>, String> {
        self.secrets.get("formatting_api_key")
    }

    pub fn tts_api_key(&self) -> Result<Option<String>, String> {
        self.secrets.get("tts_api_key")
    }

    // ── Voice ─────────────────────────────────────────────
    pub fn tts_enabled(&self) -> bool {
        self.flag_on_by_default("tts_enabled")
    }

    /// Blank means "use the provider's own default", so blank reads as unset.
    pub fn tts_model(&self) -> Option<String> {
        self.non_empty("tts_model")
    }

    pub fn tts_voice(&self) -> Option<String> {
        self.non_empty("tts_voice")
    }

    pub fn tts_response_format(&self) -> String {
        self.stored("tts_response_format")
            .unwrap_or_else(|| DEFAULT_TTS_RESPONSE_FORMAT.to_string())
            .to_ascii_lowercase()
    }

    // ── Capture and insertion ─────────────────────────────
    pub fn microphone(&self) -> Option<String> {
        self.stored("microphone")
    }

    pub fn insert_method(&self) -> InsertMethod {
        InsertMethod::from_setting(self.stored("insert_method"))
    }

    pub fn clipboard_policy(&self) -> ClipboardPolicy {
        ClipboardPolicy::from_setting(self.stored("preserve_clipboard"))
    }

    pub fn preserve_clipboard(&self) -> bool {
        self.clipboard_policy() == ClipboardPolicy::Restore
    }

    // ── Local runner ──────────────────────────────────────
    /// Where transcription happens. `remote` unless the user chose otherwise,
    /// because the local runner needs a Python and a model download first.
    pub fn transcription_backend(&self) -> TranscriptionBackend {
        match self.stored("transcription_backend").as_deref() {
            Some("local") => TranscriptionBackend::Local,
            _ => TranscriptionBackend::Remote,
        }
    }

    pub fn is_local_backend(&self) -> bool {
        self.transcription_backend() == TranscriptionBackend::Local
    }

    /// Which local model, as a key into [`crate::runner::LOCAL_MODELS`].
    pub fn local_model(&self) -> String {
        self.stored("local_model")
            .unwrap_or_else(|| DEFAULT_LOCAL_MODEL.to_string())
    }

    /// How long the sidecar may sit loaded before it drops the weights.
    ///
    /// Clamped rather than trusted: zero would unload between the prewarm and
    /// the dictation it was warming for, and an unbounded value would leave
    /// gigabytes resident for a value the user cannot see the effect of.
    pub fn local_idle_minutes(&self) -> u64 {
        self.stored("local_idle_minutes")
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(DEFAULT_LOCAL_IDLE_MINUTES)
            .clamp(1, 240)
    }

    /// Whether every request has to stay on this machine.
    ///
    /// The guard that enforces it lives in [`crate::transcribe`] and is armed
    /// by [`Settings::new`] and [`Settings::set`], so the toggle binds the next
    /// request rather than the next launch.
    ///
    /// Off until the user writes "true", but a store that cannot be read is not
    /// a store that said "off". The engine re-arms the guard from this answer
    /// on every take, so a settings table that has stopped answering would
    /// otherwise take the switch off mid-session and start letting audio leave
    /// the machine -- with nothing on the path able to say so.
    pub fn local_only(&self) -> bool {
        match self.db.get_setting("local_only") {
            Ok(stored) => stored.as_deref() == Some("true"),
            Err(_) => true,
        }
    }

    // ── History ───────────────────────────────────────────
    /// On until the user writes "false", and off when the answer cannot be
    /// read: the same reasoning as `local_only` in the other direction, since
    /// what gets written here is whatever the user said out loud.
    pub fn save_history(&self) -> bool {
        match self.db.get_setting("save_history") {
            Ok(stored) => stored.as_deref() != Some("false"),
            Err(_) => false,
        }
    }

    /// `None` means keep everything.
    pub fn history_retention_days(&self) -> Option<i64> {
        self.stored("history_retention_days")
            .and_then(|value| value.parse::<i64>().ok())
    }

    // ── Hotkeys ───────────────────────────────────────────
    /// The binding for `action` as a string, falling back to its default.
    pub fn hotkey(&self, action: &str) -> Option<String> {
        let default = hotkey::default_shortcut(action)?;
        Some(
            self.stored(&hotkey::setting_key(action))
                .unwrap_or_else(|| default.to_string()),
        )
    }

    /// The binding for `action`, parsed. A stored string that no longer parses
    /// degrades to the default rather than leaving the action unbound.
    pub fn shortcut(&self, action: &str) -> Result<HotKey, String> {
        let default =
            hotkey::default_shortcut(action).ok_or("Unknown hotkey action".to_string())?;
        let shortcut_str = self
            .stored(&hotkey::setting_key(action))
            .unwrap_or_else(|| default.to_string());
        hotkey::parse_shortcut(&shortcut_str).or_else(|_| hotkey::parse_shortcut(default))
    }

    pub fn set_hotkey(&self, action: &str, shortcut_str: &str) -> Result<(), String> {
        self.db
            .set_setting(&hotkey::setting_key(action), shortcut_str)
    }

    // ── Appearance ────────────────────────────────────────
    /// `None` means follow the system, which is what the UI does when the key
    /// holds neither "dark" nor "light".
    pub fn theme(&self) -> Option<String> {
        self.stored("theme")
            .filter(|value| value == "dark" || value == "light")
    }

    pub fn overlay_only_while_recording(&self) -> bool {
        self.flag_off_by_default("overlay_only_while_recording")
    }

    pub fn overlay_position(&self) -> String {
        self.stored("overlay_position")
            .unwrap_or_else(|| DEFAULT_OVERLAY_POSITION.to_string())
    }

    // ── Onboarding ────────────────────────────────────────
    /// Setup counts as done once a provider is saved. A self-hosted endpoint
    /// legitimately has no key, so the key cannot be the signal -- and a user
    /// who chose on-device transcription has no provider at all, so choosing
    /// that counts too.
    pub fn onboarding_complete(&self) -> bool {
        self.stored("provider").is_some() || self.is_local_backend()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_settings() -> Settings {
        let dir = std::env::temp_dir().join(format!("openflow-settings-{}", uuid::Uuid::new_v4()));
        let db = Database::new(dir.clone()).expect("a scratch database");
        Settings::new(db, SecretStore::new(dir))
    }

    #[test]
    fn live_preview_defaults_to_the_endpoint_that_is_free_to_ask() {
        let lan = Provider::from_str("custom:http://192.168.1.2:8882/v1");
        let hosted = Provider::from_str("groq");
        assert!(lan.is_custom());

        // Unset: on for the LAN box, off for anything that bills per request.
        assert!(live_preview_allowed(None, &lan));
        assert!(!live_preview_allowed(None, &hosted));

        // Set: the user's choice wins for either kind of endpoint.
        assert!(live_preview_allowed(Some("true"), &hosted));
        assert!(!live_preview_allowed(Some("false"), &lan));

        // Anything else is not a yes.
        assert!(!live_preview_allowed(Some(""), &hosted));
        assert!(!live_preview_allowed(Some("yes"), &hosted));
    }

    #[test]
    fn secret_keys_are_exactly_the_three_credentials() {
        assert!(is_secret_setting("api_key"));
        assert!(is_secret_setting("formatting_api_key"));
        assert!(is_secret_setting("tts_api_key"));
        assert!(!is_secret_setting("provider"));
        assert!(!is_secret_setting("dictionary"));
        // A near miss must not be routed to the keychain by accident.
        assert!(!is_secret_setting("api_key "));
    }

    /// An unset key has to behave like the settings UI's own default, or a
    /// fresh install and a visited-Settings install would transcribe
    /// differently.
    #[test]
    fn unset_keys_fall_back_to_the_shipped_defaults() {
        let settings = scratch_settings();

        assert_eq!(settings.provider_name(), "groq");
        assert_eq!(settings.formatting_provider_name(), None);
        assert_eq!(settings.tts_provider_name(), "openrouter");
        assert!(settings.same_provider());
        assert!(settings.format_enabled());
        assert!(settings.tts_enabled());
        assert!(settings.save_history());
        assert!(settings.preserve_clipboard());
        assert_eq!(settings.clipboard_policy(), ClipboardPolicy::Restore);
        assert_eq!(settings.insert_method(), InsertMethod::Paste);
        assert!(!settings.overlay_only_while_recording());
        assert_eq!(settings.overlay_position(), "left-center");
        assert_eq!(settings.tts_response_format(), "mp3");
        assert_eq!(settings.hotkey("record").as_deref(), Some("Option+V"));
        assert_eq!(settings.hotkey("recopy").as_deref(), Some("Ctrl+Shift+V"));
        assert_eq!(settings.hotkey("nonsense"), None);
        assert_eq!(settings.theme(), None);
        assert_eq!(settings.history_retention_days(), None);
        assert_eq!(settings.stt_model(), None);
        assert_eq!(settings.chat_model(), None);
        assert_eq!(settings.language(), None);
        assert_eq!(settings.dictionary(), None);
        assert_eq!(settings.microphone(), None);
        assert!(!settings.onboarding_complete());
        assert_eq!(
            settings.transcription_backend(),
            TranscriptionBackend::Remote
        );
        assert_eq!(settings.local_model(), DEFAULT_LOCAL_MODEL);
        assert_eq!(settings.local_idle_minutes(), DEFAULT_LOCAL_IDLE_MINUTES);
        assert!(!settings.local_only());
    }

    /// The four keys the local runner reads, including the two that have to be
    /// bounded rather than believed.
    #[test]
    fn the_local_runner_settings_round_trip_and_stay_in_range() {
        let settings = scratch_settings();
        settings
            .set("transcription_backend", "local")
            .expect("write the backend");
        assert!(settings.is_local_backend());
        assert!(
            settings.onboarding_complete(),
            "choosing on-device transcription is a finished setup, with no provider row"
        );
        settings
            .set("local_model", "fast")
            .expect("write the model");
        assert_eq!(settings.local_model(), "fast");

        settings.set("local_idle_minutes", "25").expect("write it");
        assert_eq!(settings.local_idle_minutes(), 25);
        // Zero would unload the model between the prewarm and the dictation it
        // was warming for; an enormous value would pin gigabytes forever.
        settings.set("local_idle_minutes", "0").expect("write it");
        assert_eq!(settings.local_idle_minutes(), 1);
        settings
            .set("local_idle_minutes", "99999")
            .expect("write it");
        assert_eq!(settings.local_idle_minutes(), 240);
        settings
            .set("local_idle_minutes", "soon")
            .expect("write it");
        assert_eq!(settings.local_idle_minutes(), DEFAULT_LOCAL_IDLE_MINUTES);

        // Anything that is not the literal "local" is remote: a half-written
        // value must not silently point dictation at a runner that is not there.
        settings
            .set("transcription_backend", "on-this-mac")
            .expect("write it");
        assert_eq!(
            settings.transcription_backend(),
            TranscriptionBackend::Remote
        );
    }

    #[test]
    fn stored_values_win_over_the_defaults() {
        let settings = scratch_settings();
        settings.set("provider", "openai").expect("write provider");
        settings.set("same_provider", "false").expect("write flag");
        settings.set("format_enabled", "false").expect("write flag");
        settings.set("save_history", "false").expect("write flag");
        settings
            .set("preserve_clipboard", "false")
            .expect("write flag");
        settings.set("insert_method", "type").expect("write method");
        settings
            .set("overlay_only_while_recording", "true")
            .expect("write flag");
        settings
            .set("history_retention_days", "30")
            .expect("write retention");
        settings
            .set("tts_response_format", "WAV")
            .expect("write format");
        settings
            .set("hotkey_record", "Cmd+Shift+K")
            .expect("write hotkey");
        settings.set("theme", "light").expect("write theme");

        assert_eq!(settings.provider_name(), "openai");
        assert!(matches!(settings.provider(), Provider::OpenAI));
        assert!(!settings.same_provider());
        assert!(!settings.format_enabled());
        assert!(!settings.save_history());
        assert!(!settings.preserve_clipboard());
        assert_eq!(settings.clipboard_policy(), ClipboardPolicy::Keep);
        assert_eq!(settings.insert_method(), InsertMethod::Type);
        assert!(settings.overlay_only_while_recording());
        assert_eq!(settings.history_retention_days(), Some(30));
        assert_eq!(settings.tts_response_format(), "wav");
        assert_eq!(settings.hotkey("record").as_deref(), Some("Cmd+Shift+K"));
        assert_eq!(settings.theme().as_deref(), Some("light"));
        assert!(settings.onboarding_complete());
        // Unset still means "the transcription provider" for formatting.
        assert!(matches!(settings.formatting_provider(), Provider::OpenAI));
    }

    /// A binding the user saved that no longer parses must not leave the action
    /// dead: it falls back to the shipped default.
    #[test]
    fn an_unparseable_binding_falls_back_to_the_default() {
        let settings = scratch_settings();
        settings
            .set("hotkey_record", "Option+NotAKey")
            .expect("write hotkey");
        assert_eq!(
            settings.shortcut("record").expect("a parsed shortcut"),
            hotkey::parse_shortcut("Option+V").expect("the default parses")
        );
        assert!(settings.shortcut("nonsense").is_err());
    }

    /// Storing the row is not the feature; arming the guard is. Writing
    /// `local_only` has to bind the very next request, so this checks the
    /// network guard and not just the value that comes back out of the table.
    #[test]
    fn turning_local_only_on_arms_the_network_guard() {
        let _serialized = crate::transcribe::tests::LOCAL_ONLY_TESTS
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let settings = scratch_settings();
        assert!(!settings.local_only(), "off until the user says otherwise");

        settings
            .set("local_only", "true")
            .expect("write the toggle");
        assert!(settings.local_only());
        assert!(
            crate::transcribe::local_only(),
            "the write must arm the guard, not only store a row"
        );

        settings
            .set("local_only", "false")
            .expect("write the toggle");
        assert!(!crate::transcribe::local_only());

        // ...and a launch with the row already on arms it before any request.
        let dir =
            std::env::temp_dir().join(format!("openflow-local-only-{}", uuid::Uuid::new_v4()));
        let db = Database::new(dir.clone()).expect("a scratch database");
        db.set_setting("local_only", "true").expect("seed the row");
        let reopened = Settings::new(db, SecretStore::new(dir));
        assert!(reopened.local_only());
        assert!(
            crate::transcribe::local_only(),
            "a stored toggle must arm the guard at construction"
        );
        reopened
            .set("local_only", "false")
            .expect("leave the process as we found it");
    }

    /// The two settings that protect the user have a wrong side to fail on,
    /// and a settings table can stop answering for the rest of the process's
    /// life without anything on the read path being able to report it. Both
    /// have to end up on the side the user was promised, while the settings
    /// that only shape behaviour keep falling back to their defaults.
    #[test]
    fn a_store_that_cannot_answer_fails_toward_privacy() {
        let settings = scratch_settings();
        assert!(!settings.local_only(), "off on a fresh install");
        assert!(settings.save_history(), "on on a fresh install");

        crate::db::tests::poison_the_connection_lock(settings.db());

        assert!(
            settings.local_only(),
            "a guard that promises nothing leaves this machine cannot take itself off"
        );
        assert!(
            !settings.save_history(),
            "speech must not be written to disk on the strength of a read that failed"
        );
        assert!(
            settings.get("provider").is_err(),
            "the failure is now something a caller can see"
        );
        assert_eq!(settings.provider_name(), DEFAULT_PROVIDER);
        assert!(settings.format_enabled());
        assert_eq!(settings.local_idle_minutes(), DEFAULT_LOCAL_IDLE_MINUTES);
    }

    /// The guard is armed from the same answer at construction, and that branch
    /// is reachable in the case that matters most: the store was already
    /// unreadable before this process ever asked it anything.
    #[test]
    fn a_store_that_cannot_answer_at_startup_arms_the_guard() {
        let _serialized = crate::transcribe::tests::LOCAL_ONLY_TESTS
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        crate::transcribe::set_local_only(false);

        let dir = std::env::temp_dir().join(format!("openflow-settings-{}", uuid::Uuid::new_v4()));
        let db = Database::new(dir.clone()).expect("a scratch database");
        crate::db::tests::poison_the_connection_lock(&db);
        let settings = Settings::new(db, SecretStore::new(dir));

        assert!(
            crate::transcribe::local_only(),
            "a store that could not be read at startup has not said the guard is off"
        );
        assert!(settings.local_only());
        crate::transcribe::set_local_only(false);
    }

    /// A blank voice model or voice means "the provider's own default", not an
    /// empty model name on the wire.
    #[test]
    fn blank_voice_fields_read_as_unset() {
        let settings = scratch_settings();
        settings.set("tts_model", "   ").expect("write model");
        settings.set("tts_voice", "").expect("write voice");
        assert_eq!(settings.tts_model(), None);
        assert_eq!(settings.tts_voice(), None);
    }
}
