use base64::Engine;
use reqwest::{multipart, redirect, Method, Response};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

const OPENROUTER_URL: &str = "https://openrouter.ai/api/v1";
pub const GEMINI_TTS_MODEL: &str = "google/gemini-3.1-flash-tts-preview";

#[derive(Deserialize)]
struct WhisperResponse {
    text: String,
}

#[derive(Clone, Debug)]
pub enum Provider {
    Groq,
    OpenAI,
    OpenRouter,
    Deepgram,
    Custom { base_url: String },
}

impl Provider {
    // Public API since the move into the library crate, so clippy now compares it
    // with `std::str::FromStr`. This one is infallible and falls back to Groq;
    // the trait's `Result` signature would change every call site.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "openai" => Self::OpenAI,
            "openrouter" => Self::OpenRouter,
            "deepgram" => Self::Deepgram,
            custom if custom.starts_with("custom:") => Self::Custom {
                base_url: validate_custom_url(custom.trim_start_matches("custom:")),
            },
            _ => Self::Groq,
        }
    }

    fn base_url(&self) -> &str {
        match self {
            Self::Groq => "https://api.groq.com/openai/v1",
            Self::OpenAI => "https://api.openai.com/v1",
            Self::OpenRouter => OPENROUTER_URL,
            Self::Deepgram => "https://api.deepgram.com/v1",
            Self::Custom { base_url } => base_url.as_str(),
        }
    }

    fn endpoint(&self, suffix: &str) -> Result<String, String> {
        if self.base_url().is_empty() {
            return Err("Custom provider URL is invalid".to_string());
        }
        Ok(format!(
            "{}/{}",
            self.base_url().trim_end_matches('/'),
            suffix.trim_start_matches('/')
        ))
    }

    pub fn default_stt_model(&self) -> &str {
        match self {
            Self::Groq => "whisper-large-v3-turbo",
            Self::OpenAI => "whisper-1",
            Self::OpenRouter => "openai/whisper-1",
            Self::Deepgram => "nova-3",
            Self::Custom { .. } => "whisper-large-v3",
        }
    }

    /// Groq retired its Llama endpoints for new accounts; gpt-oss-20b is the
    /// fastest model left and, with reasoning turned down, answers immediately.
    pub fn default_chat_model(&self) -> &str {
        match self {
            Self::Groq => "openai/gpt-oss-20b",
            Self::OpenAI => "gpt-4o-mini",
            Self::OpenRouter => "google/gemini-3.1-flash-lite-preview",
            Self::Deepgram => "openai/gpt-oss-20b",
            Self::Custom { .. } => "default",
        }
    }

    pub fn default_tts_model(&self) -> &str {
        match self {
            Self::Groq => "canopylabs/orpheus-v1-english",
            Self::OpenAI | Self::Custom { .. } => "tts-1",
            _ => GEMINI_TTS_MODEL,
        }
    }

    pub fn default_tts_voice(&self) -> &str {
        match self {
            Self::Groq => "troy",
            Self::OpenAI | Self::Custom { .. } => "alloy",
            _ => "Kore",
        }
    }

    /// `None` when there is no key to send.
    ///
    /// Self-hosted OpenAI-compatible servers on a LAN (Kokoro, llama.cpp,
    /// vLLM, ...) usually run with no auth at all, and several of them reject a
    /// request carrying `Bearer ` with an empty value. Omit the header instead.
    fn authorization(&self, api_key: &str) -> Option<String> {
        if api_key.trim().is_empty() {
            return None;
        }
        Some(match self {
            Self::Deepgram => format!("Token {}", api_key),
            _ => format!("Bearer {}", api_key),
        })
    }

    pub fn is_custom(&self) -> bool {
        matches!(self, Self::Custom { .. })
    }

    pub fn is_openrouter(&self) -> bool {
        matches!(self, Self::OpenRouter)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub model_type: String,
}

pub async fn fetch_models(api_key: &str, provider: &Provider) -> Result<Vec<ModelInfo>, String> {
    ensure_api_key(api_key, provider)?;
    if matches!(provider, Provider::Deepgram) {
        return Ok(vec![
            ModelInfo {
                id: "nova-3".into(),
                name: "Nova 3".into(),
                model_type: "stt".into(),
            },
            ModelInfo {
                id: "nova-2".into(),
                name: "Nova 2".into(),
                model_type: "stt".into(),
            },
        ]);
    }
    let response = with_openrouter_headers(
        with_auth(
            request(Method::GET, &provider.endpoint("models")?)?,
            provider,
            api_key,
        ),
        provider,
    )
    .timeout(Duration::from_secs(15))
    .send()
    .await
    .map_err(|error| request_error("fetch models", error))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| format!("Could not read models response: {}", error))?;
    if !status.is_success() {
        return Err(api_error("Models", status, &body));
    }
    let json: serde_json::Value = serde_json::from_str(&body)
        .map_err(|error| format!("Could not parse models: {}", error))?;
    let data = json
        .get("data")
        .and_then(|value| value.as_array())
        .ok_or("Models response did not contain data")?;

    let mut models = Vec::new();
    for item in data {
        let Some(id) = item
            .get("id")
            .and_then(|value| value.as_str())
            .filter(|id| !id.is_empty())
        else {
            continue;
        };
        let Some(model_type) = classify_model(item, id) else {
            continue;
        };
        let name = item
            .get("name")
            .and_then(|value| value.as_str())
            .unwrap_or(id);
        models.push(ModelInfo {
            id: id.to_string(),
            name: name.to_string(),
            model_type: model_type.to_string(),
        });
    }
    if provider.is_openrouter() && !models.iter().any(|model| model.id == GEMINI_TTS_MODEL) {
        models.push(ModelInfo {
            id: GEMINI_TTS_MODEL.into(),
            name: "Gemini 3.1 Flash TTS Preview".into(),
            model_type: "tts".into(),
        });
    }
    models.sort_by(|a, b| a.model_type.cmp(&b.model_type).then(a.name.cmp(&b.name)));
    models.dedup_by(|a, b| a.id == b.id);
    Ok(models)
}

fn classify_model(item: &serde_json::Value, id: &str) -> Option<&'static str> {
    let lower = id.to_ascii_lowercase();
    let outputs = modalities(item, "output_modalities");
    let has_output = |expected: &str| outputs.iter().any(|modality| modality == expected);
    let id_tokens = lower.split(['/', '-', '_', '.', ':']);
    let is_stt_name = lower.contains("whisper")
        || lower.contains("chirp")
        || lower.contains("transcrib")
        || lower.contains("speech-to-text")
        || id_tokens.clone().any(|token| token == "stt");
    let is_tts_name = lower.contains("text-to-speech")
        || lower.contains("speech-generation")
        || id_tokens.clone().any(|token| token == "tts");

    if has_output("transcription") || is_stt_name {
        return Some("stt");
    }
    if has_output("speech") || is_tts_name {
        return Some("tts");
    }
    if lower.contains("embed")
        || lower.contains("moderation")
        || lower.contains("rerank")
        || lower.contains("image-gen")
        || lower.contains("dall-")
        || lower.contains("realtime")
    {
        return None;
    }
    // Model APIs are inconsistent about architecture metadata. Unknown text and
    // multimodal models remain valid formatting candidates unless their id
    // clearly identifies a specialized non-chat model above.
    Some("chat")
}

fn modalities(item: &serde_json::Value, key: &str) -> Vec<String> {
    item.get("architecture")
        .and_then(|value| value.get(key))
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
        .filter_map(|value| value.as_str().map(str::to_ascii_lowercase))
        .collect()
}

pub async fn transcribe_audio(
    wav_bytes: Vec<u8>,
    api_key: &str,
    language: Option<&str>,
    provider: &Provider,
    model: Option<&str>,
    dictionary: Option<&str>,
) -> Result<String, String> {
    ensure_api_key(api_key, provider)?;
    if wav_bytes.len() > 50 * 1024 * 1024 {
        return Err("Recording is too large to transcribe".to_string());
    }
    match provider {
        Provider::Deepgram => transcribe_deepgram(wav_bytes, api_key, language, model).await,
        Provider::OpenRouter => transcribe_openrouter(wav_bytes, api_key, language, model).await,
        _ => transcribe_whisper(wav_bytes, api_key, language, provider, model, dictionary).await,
    }
}

async fn transcribe_openrouter(
    wav_bytes: Vec<u8>,
    api_key: &str,
    language: Option<&str>,
    model: Option<&str>,
) -> Result<String, String> {
    let mut body = serde_json::json!({
        "model": model.unwrap_or("openai/whisper-1"),
        "input_audio": { "data": base64::engine::general_purpose::STANDARD.encode(wav_bytes), "format": "wav" }
    });
    if let Some(language) = normalize_language(language) {
        body["language"] = language.into();
    }
    let response = with_openrouter_headers(
        request(
            Method::POST,
            &format!("{OPENROUTER_URL}/audio/transcriptions"),
        )?
        .bearer_auth(api_key),
        &Provider::OpenRouter,
    )
    .json(&body)
    .timeout(Duration::from_secs(90))
    .send()
    .await
    .map_err(|error| request_error("transcribe audio", error))?;
    parse_transcription_response(response, "OpenRouter").await
}

async fn transcribe_whisper(
    wav_bytes: Vec<u8>,
    api_key: &str,
    language: Option<&str>,
    provider: &Provider,
    model: Option<&str>,
    dictionary: Option<&str>,
) -> Result<String, String> {
    let file = multipart::Part::bytes(wav_bytes)
        .file_name("audio.wav")
        .mime_str("audio/wav")
        .map_err(|error| format!("Could not prepare recording: {}", error))?;
    let mut form = multipart::Form::new()
        .part("file", file)
        .text(
            "model",
            model.unwrap_or(provider.default_stt_model()).to_string(),
        )
        .text("response_format", "json");
    if let Some(language) = normalize_language(language) {
        form = form.text("language", language.to_string());
    }
    // Whisper reads `prompt` as preceding text and copies its spellings, so a
    // list of names and terms is the cheapest personal dictionary there is.
    if let Some(dictionary) = dictionary_prompt(dictionary) {
        form = form.text("prompt", dictionary);
    }
    let response = with_openrouter_headers(
        with_auth(
            request(Method::POST, &provider.endpoint("audio/transcriptions")?)?,
            provider,
            api_key,
        ),
        provider,
    )
    .multipart(form)
    .timeout(Duration::from_secs(90))
    .send()
    .await
    .map_err(|error| request_error("transcribe audio", error))?;
    parse_transcription_response(response, "Transcription").await
}

async fn parse_transcription_response(response: Response, label: &str) -> Result<String, String> {
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| format!("Could not read transcription response: {}", error))?;
    if !status.is_success() {
        return Err(api_error(label, status, &body));
    }
    let result: WhisperResponse = serde_json::from_str(&body)
        .map_err(|error| format!("Could not parse transcription: {}", error))?;
    let text = result.text.trim().to_string();
    if text.is_empty() {
        Err("The transcription service returned no speech".to_string())
    } else {
        Ok(text)
    }
}

async fn transcribe_deepgram(
    wav_bytes: Vec<u8>,
    api_key: &str,
    language: Option<&str>,
    model: Option<&str>,
) -> Result<String, String> {
    let mut url = reqwest::Url::parse("https://api.deepgram.com/v1/listen")
        .map_err(|error| format!("Invalid Deepgram URL: {}", error))?;
    url.query_pairs_mut()
        .append_pair("model", model.unwrap_or("nova-3"))
        .append_pair("smart_format", "true");
    if let Some(language) = normalize_language(language) {
        url.query_pairs_mut().append_pair("language", language);
    }
    let response = request(Method::POST, url.as_str())?
        .header("Authorization", format!("Token {}", api_key))
        .header("Content-Type", "audio/wav")
        .body(wav_bytes)
        .timeout(Duration::from_secs(90))
        .send()
        .await
        .map_err(|error| request_error("transcribe audio", error))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| format!("Could not read Deepgram response: {}", error))?;
    if !status.is_success() {
        return Err(api_error("Deepgram", status, &body));
    }
    let result: serde_json::Value = serde_json::from_str(&body)
        .map_err(|error| format!("Could not parse Deepgram response: {}", error))?;
    result
        .pointer("/results/channels/0/alternatives/0/transcript")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .ok_or("Deepgram returned no speech".to_string())
}

const FORMAT_SYSTEM_PROMPT: &str = "Format this dictation into clean text. Add punctuation, capitalization, and paragraph breaks. Interpret spoken editing commands such as new paragraph, comma, scratch that, undo, and all caps. Remove accidental filler words. Never add content that was not spoken. Output only the formatted text.";

pub async fn format_text(
    raw_text: &str,
    api_key: &str,
    context: Option<&str>,
    provider: &Provider,
    chat_model: Option<&str>,
) -> Result<String, String> {
    ensure_api_key(api_key, provider)?;
    if matches!(provider, Provider::Deepgram) {
        return Err("Deepgram provides transcription but not chat formatting. Choose a separate formatting provider or disable formatting.".to_string());
    }
    let user_content = context
        .filter(|ctx| !ctx.trim().is_empty())
        .map(|ctx| format!("Context: {}\n\nDictation: {}", ctx, raw_text))
        .unwrap_or_else(|| raw_text.to_string());
    let model = chat_model.unwrap_or(provider.default_chat_model());
    let mut body = serde_json::json!({
        "model": model,
        "messages": [{"role":"system","content":FORMAT_SYSTEM_PROMPT},{"role":"user","content":user_content}],
        "temperature": 0.3, "max_tokens": 2048
    });
    if let Some(effort) = reasoning_effort(provider, model) {
        body["reasoning_effort"] = effort.into();
    }
    let endpoint = provider.endpoint("chat/completions")?;
    let response = with_openrouter_headers(
        with_auth(request(Method::POST, &endpoint)?, provider, api_key),
        provider,
    )
    .json(&body)
    .timeout(Duration::from_secs(45))
    .send()
    .await
    .map_err(|error| request_error("format text", error))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| format!("Could not read formatting response: {}", error))?;
    if !status.is_success() {
        return Err(api_error("Formatting", status, &body));
    }
    let result: serde_json::Value = serde_json::from_str(&body)
        .map_err(|error| format!("Could not parse formatting response: {}", error))?;
    result
        .pointer("/choices/0/message/content")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .ok_or("Formatting service returned no text".to_string())
}

pub async fn request_speech(
    text: &str,
    api_key: &str,
    provider: &Provider,
    model: &str,
    voice: &str,
    response_format: &str,
) -> Result<Response, String> {
    ensure_api_key(api_key, provider)?;
    if text.trim().is_empty() {
        return Err("Text to speak cannot be empty".to_string());
    }
    if text.chars().count() > 32_000 {
        return Err("Text to speak is too long (maximum 32,000 characters)".to_string());
    }
    if !matches!(response_format, "mp3" | "pcm" | "wav") {
        return Err("Speech format must be mp3, pcm, or wav".to_string());
    }
    if voice.is_empty()
        || voice.len() > 80
        || !voice
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        return Err("Speech voice is invalid".to_string());
    }
    if matches!(provider, Provider::Deepgram) {
        return Err("Deepgram does not expose an OpenAI-compatible speech endpoint".to_string());
    }
    let body = serde_json::json!({ "model": model, "input": text, "voice": voice, "response_format": response_format });
    let response = with_openrouter_headers(
        with_auth(
            request(Method::POST, &provider.endpoint("audio/speech")?)?,
            provider,
            api_key,
        ),
        provider,
    )
    .json(&body)
    .timeout(Duration::from_secs(120))
    .send()
    .await
    .map_err(|error| request_error("generate speech", error))?;
    if response.status().is_success() {
        validate_speech_response(&response)?;
        return Ok(response);
    }
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    Err(api_error("Speech", status, &body))
}

fn validate_speech_response(response: &Response) -> Result<(), String> {
    if response.content_length() == Some(0) {
        return Err("Speech provider returned an empty audio response".to_string());
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value
                .split(';')
                .next()
                .unwrap_or(value)
                .trim()
                .to_ascii_lowercase()
        })
        .ok_or_else(|| "Speech provider did not identify its response as audio".to_string())?;
    if !content_type.starts_with("audio/") && content_type != "application/octet-stream" {
        return Err(format!(
            "Speech provider returned {} instead of audio",
            content_type
        ));
    }
    Ok(())
}

pub fn speech_mime(format: &str) -> &'static str {
    match format {
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        _ => "audio/pcm",
    }
}

// ── Local only ────────────────────────────────────────────

/// Whether every request this process makes has to stay on this machine.
///
/// Mirrors the `local_only` setting. It is a process-wide flag rather than a
/// parameter because it is an invariant about the *client*, not about one call:
/// the toggle promises "no requests leave this machine", and a promise that
/// each call site has to remember to pass along is one an added call site can
/// quietly break. [`Settings::set`](crate::settings::Settings::set) keeps it in
/// step with the stored value, and the pipeline re-reads it before every take.
static LOCAL_ONLY: AtomicBool = AtomicBool::new(false);

/// Point the guard at the stored setting.
pub fn set_local_only(on: bool) {
    LOCAL_ONLY.store(on, Ordering::SeqCst);
}

pub fn local_only() -> bool {
    LOCAL_ONLY.load(Ordering::SeqCst)
}

/// Whether a URL names this machine.
///
/// Deliberately the three literal hosts and nothing else: `127.0.0.1`,
/// `::1` (in either spelling) and `localhost`. The rest of `127.0.0.0/8` is
/// equally local, and a hostname can resolve to a loopback address, but neither
/// is worth widening the rule for -- the runner binds `127.0.0.1` and a
/// privacy switch is the wrong place to be generous. A URL that does not parse
/// is not loopback either.
pub fn is_loopback_url(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    matches!(
        host.to_ascii_lowercase().as_str(),
        "127.0.0.1" | "localhost" | "::1" | "[::1]"
    )
}

/// Refuse a request that would leave this machine while `local_only` is on.
///
/// Called before the request is built, so nothing is connected, no DNS is
/// resolved and no credential is attached: the refusal happens with the URL and
/// the setting alone.
fn guard_local_only(url: &str) -> Result<(), String> {
    if !local_only() || is_loopback_url(url) {
        return Ok(());
    }
    let host = reqwest::Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_string))
        .unwrap_or_else(|| "another machine".to_string());
    Err(format!(
        "\"Local only\" is on, so OpenFlow did not send this to {}. Turn Local only off in Settings, or point this at a service on this Mac.",
        host
    ))
}

/// The one way to start a request in this module.
///
/// Every call goes through here, which is what makes the `local_only` guard an
/// invariant rather than a habit: a new endpoint cannot be added without being
/// checked, because there is no other way to reach the client.
fn request(method: Method, url: &str) -> Result<reqwest::RequestBuilder, String> {
    guard_local_only(url)?;
    Ok(client()?.request(method, url))
}

/// One client for the life of the process. Building a `reqwest::Client`
/// constructs a TLS configuration and a connection pool each time; doing that
/// per request cost a fresh TCP and TLS handshake on every call and threw the
/// pool away before the next one could reuse it.
fn client() -> Result<reqwest::Client, String> {
    static CLIENT: OnceLock<Result<reqwest::Client, String>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .pool_idle_timeout(Duration::from_secs(90))
                .user_agent("OpenFlow/0.1")
                .redirect(redirect::Policy::custom(guard_redirect))
                .build()
                .map_err(|error| format!("Could not initialize network client: {}", error))
        })
        .clone()
}

/// The `local_only` rule again, one hop later.
///
/// Checking the URL the caller asked for is not enough: a loopback endpoint
/// that answers 307 or 308 with a `Location` off this machine would have the
/// client re-send the whole recording there, and the check at
/// [`guard_local_only`] has already passed. That is the same audio leaving the
/// Mac, by consent the user never gave. So every hop is checked, not just the
/// first.
///
/// The default policy is `limited(10)`, and setting a custom one replaces it,
/// so the hop limit is re-stated here rather than lost.
fn guard_redirect(attempt: redirect::Attempt) -> redirect::Action {
    if local_only() && !is_loopback_url(attempt.url().as_str()) {
        let host = attempt
            .url()
            .host_str()
            .unwrap_or("another machine")
            .to_string();
        // `error` rather than `stop`: stopping hands the 307 back as if it were
        // the answer, and the caller reports a parse failure instead of the
        // reason. `request_error` unwraps this message.
        return attempt.error(format!(
            "\"Local only\" is on, so OpenFlow did not follow a redirect to {}. Turn Local only off in Settings, or point this at a service on this Mac.",
            host
        ));
    }
    if attempt.previous().len() >= 10 {
        return attempt.error("too many redirects");
    }
    attempt.follow()
}

/// Attaches the Authorization header only when there is a key to attach.
fn with_auth(
    builder: reqwest::RequestBuilder,
    provider: &Provider,
    api_key: &str,
) -> reqwest::RequestBuilder {
    match provider.authorization(api_key) {
        Some(value) => builder.header("Authorization", value),
        None => builder,
    }
}

fn with_openrouter_headers(
    builder: reqwest::RequestBuilder,
    provider: &Provider,
) -> reqwest::RequestBuilder {
    if provider.is_openrouter() {
        builder
            .header("HTTP-Referer", "https://github.com/laisyio/openflow")
            .header("X-Title", "OpenFlow")
    } else {
        builder
    }
}

/// gpt-oss models think before they answer unless told not to. Cleanup needs
/// none of that, and Groq only accepts the parameter for these models.
fn reasoning_effort(provider: &Provider, model: &str) -> Option<&'static str> {
    (matches!(provider, Provider::Groq) && model.contains("gpt-oss")).then_some("low")
}

/// Whisper caps the prompt at 224 tokens; 800 characters stays well under it.
fn dictionary_prompt(dictionary: Option<&str>) -> Option<String> {
    let text = dictionary?.trim();
    if text.is_empty() {
        return None;
    }
    Some(text.chars().take(800).collect())
}

fn normalize_language(language: Option<&str>) -> Option<&str> {
    language
        .map(str::trim)
        .filter(|language| !language.is_empty() && *language != "auto")
}

/// Self-hosted servers on a LAN usually run without auth, so an empty key is
/// only an error for hosted providers. Saying so early beats a 401 later.
fn ensure_api_key(api_key: &str, provider: &Provider) -> Result<(), String> {
    if api_key.len() > 16_384 {
        return Err("API key is unexpectedly large".to_string());
    }
    if api_key.trim().is_empty() && !provider.is_custom() {
        return Err("No API key set. Add one in Settings.".to_string());
    }
    Ok(())
}

fn api_error(label: &str, status: reqwest::StatusCode, body: &str) -> String {
    match status.as_u16() {
        401 | 403 => format!(
            "{} rejected the API key. Check the key and provider.",
            label
        ),
        402 => format!("{} account has insufficient credits.", label),
        408 => format!("{} request timed out.", label),
        413 => format!("{} rejected the recording because it is too large.", label),
        429 => format!("{} rate limit reached. Wait briefly and try again.", label),
        _ => {
            let detail = serde_json::from_str::<serde_json::Value>(body)
                .ok()
                .and_then(|json| {
                    json.pointer("/error/message")
                        .and_then(|value| value.as_str())
                        .map(str::to_string)
                })
                .unwrap_or_else(|| body.chars().take(300).collect());
            if detail.trim().is_empty() {
                format!("{} API returned {}", label, status)
            } else {
                format!("{} API returned {}: {}", label, status, detail.trim())
            }
        }
    }
}

fn request_error(action: &str, error: reqwest::Error) -> String {
    if error.is_timeout() {
        format!("Timed out while trying to {}", action)
    } else if error.is_connect() {
        format!("Could not connect to the provider to {}", action)
    } else if error.is_redirect() {
        // reqwest's own Display for a redirect failure names the URL and not
        // the reason, and the reason is the whole message here: the redirect
        // policy refused a hop off this machine. Dig it out of the chain.
        format!("Could not {}: {}", action, deepest_cause(&error))
    } else {
        format!("Could not {}: {}", action, error)
    }
}

/// The message furthest down an error's `source` chain, which is where a
/// policy's own words end up once reqwest has wrapped them.
fn deepest_cause(error: &dyn std::error::Error) -> String {
    let mut deepest = error.to_string();
    let mut current = error.source();
    while let Some(cause) = current {
        deepest = cause.to_string();
        current = cause.source();
    }
    deepest
}

fn validate_custom_url(value: &str) -> String {
    let Ok(mut url) = reqwest::Url::parse(value.trim()) else {
        return String::new();
    };
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return String::new();
    }
    url.set_query(None);
    url.set_fragment(None);
    url.to_string().trim_end_matches('/').to_string()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn empty_api_key_sends_no_authorization_header() {
        // Self-hosted servers on a LAN are usually unauthenticated, and some
        // reject a request carrying `Bearer ` with an empty value.
        let custom = Provider::from_str("custom:http://192.168.1.10:8880/v1");
        assert_eq!(custom.authorization(""), None);
        assert_eq!(custom.authorization("   "), None);
        assert_eq!(
            custom.authorization("sk-local"),
            Some("Bearer sk-local".to_string())
        );
        assert_eq!(
            Provider::from_str("deepgram").authorization("abc"),
            Some("Token abc".to_string())
        );
    }

    #[test]
    fn custom_provider_accepts_plain_http_lan_addresses() {
        // No TLS on a home network, so http:// has to survive validation.
        let provider = Provider::from_str("custom:http://192.168.100.121:8880/v1");
        assert!(provider.is_custom());
        assert_eq!(
            provider.endpoint("audio/speech").unwrap(),
            "http://192.168.100.121:8880/v1/audio/speech"
        );
    }

    #[test]
    fn custom_provider_with_no_url_reports_a_usable_error() {
        let broken = Provider::from_str("custom:not a url");
        assert!(broken.endpoint("audio/speech").is_err());
    }

    #[test]
    fn custom_provider_rejects_non_http_urls() {
        assert!(
            matches!(Provider::from_str("custom:file:///tmp/api"), Provider::Custom { base_url } if base_url.is_empty())
        );
        assert!(
            matches!(Provider::from_str("custom:https://localhost:8080/v1/"), Provider::Custom { base_url } if base_url == "https://localhost:8080/v1")
        );
    }

    #[test]
    fn empty_api_key_is_allowed_only_for_custom_endpoints() {
        let custom = Provider::from_str("custom:http://192.168.1.10:8880/v1");
        assert!(ensure_api_key("", &custom).is_ok());
        assert!(ensure_api_key("   ", &custom).is_ok());
        assert!(ensure_api_key("", &Provider::OpenRouter).is_err());
        assert!(ensure_api_key("sk-x", &Provider::OpenRouter).is_ok());
        assert!(ensure_api_key(&"x".repeat(16_385), &custom).is_err());
    }

    #[test]
    fn reasoning_is_turned_down_only_for_gpt_oss_on_groq() {
        assert_eq!(
            reasoning_effort(&Provider::Groq, "openai/gpt-oss-20b"),
            Some("low")
        );
        assert_eq!(reasoning_effort(&Provider::Groq, "qwen/qwen3.6-27b"), None);
        assert_eq!(
            reasoning_effort(&Provider::OpenRouter, "openai/gpt-oss-20b"),
            None
        );
    }

    #[test]
    fn dictionary_prompt_is_trimmed_and_bounded() {
        assert_eq!(dictionary_prompt(None), None);
        assert_eq!(dictionary_prompt(Some("   ")), None);
        assert_eq!(
            dictionary_prompt(Some(" ENTRO.LY, FastPay ")),
            Some("ENTRO.LY, FastPay".to_string())
        );
        assert_eq!(
            dictionary_prompt(Some(&"x".repeat(2_000))).map(|p| p.len()),
            Some(800)
        );
    }

    #[test]
    fn groq_speech_defaults_point_at_orpheus() {
        assert_eq!(
            Provider::Groq.default_tts_model(),
            "canopylabs/orpheus-v1-english"
        );
        assert_eq!(Provider::Groq.default_tts_voice(), "troy");
        assert_eq!(Provider::OpenRouter.default_tts_model(), GEMINI_TTS_MODEL);
    }

    #[test]
    fn auto_language_is_omitted() {
        assert_eq!(normalize_language(Some("auto")), None);
        assert_eq!(normalize_language(Some("en")), Some("en"));
    }

    #[test]
    fn classifies_models_when_modality_metadata_is_missing() {
        let no_architecture = serde_json::json!({});
        assert_eq!(
            classify_model(&no_architecture, "google/gemini-3.1-flash-tts-preview"),
            Some("tts")
        );
        assert_eq!(
            classify_model(&no_architecture, "openai/whisper-1"),
            Some("stt")
        );
        assert_eq!(
            classify_model(&no_architecture, "google/gemini-3.1-flash-lite-preview"),
            Some("chat")
        );
        assert_eq!(
            classify_model(&no_architecture, "openai/text-embedding-3-small"),
            None
        );
    }

    /// The rule the Local only toggle promises, on its own.
    #[test]
    fn only_this_machine_counts_as_loopback() {
        for url in [
            "http://127.0.0.1:8123/v1",
            "http://localhost:8123/v1",
            "http://LocalHost:8123/v1",
            "http://[::1]:8123/v1",
            "https://127.0.0.1/v1/audio/transcriptions",
        ] {
            assert!(is_loopback_url(url), "{url} names this machine");
        }
        for url in [
            "https://api.groq.com/openai/v1",
            "http://192.168.1.10:8880/v1",
            "http://127.0.0.2:8123/v1",
            "http://localhost.evil.example/v1",
            "http://127.0.0.1.evil.example/v1",
            "not a url",
            "",
        ] {
            assert!(!is_loopback_url(url), "{url} does not name this machine");
        }
    }

    /// The guard itself: a hosted URL is refused, with the setting named, and a
    /// loopback URL passes. Refused *before* the request is built, so nothing
    /// resolves, connects or attaches a credential.
    #[test]
    fn local_only_refuses_a_hosted_url_and_passes_a_loopback_one() {
        let _guard = LOCAL_ONLY_TESTS
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        set_local_only(false);
        assert!(guard_local_only("https://api.groq.com/openai/v1").is_ok());

        set_local_only(true);
        let refused = guard_local_only("https://api.groq.com/openai/v1/audio/transcriptions")
            .expect_err("a hosted URL must be refused while Local only is on");
        assert!(
            refused.contains("Local only"),
            "the error must name the setting: {refused}"
        );
        assert!(
            refused.contains("api.groq.com"),
            "the error must name the host it refused: {refused}"
        );
        assert!(guard_local_only("http://127.0.0.1:8123/v1").is_ok());
        assert!(guard_local_only("http://localhost:8123/v1").is_ok());
        set_local_only(false);
    }

    /// ...and the guard is actually on the path every request takes, for
    /// transcription, cleanup and voice alike. A unit test of the predicate
    /// would pass even if nothing called it.
    #[test]
    fn local_only_stops_transcription_cleanup_and_voice_before_they_connect() {
        let _guard = LOCAL_ONLY_TESTS
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let runtime = tokio::runtime::Runtime::new().expect("test runtime");
        let hosted = Provider::Groq;
        let wav = vec![0u8; 64];

        set_local_only(true);
        for error in [
            runtime
                .block_on(transcribe_audio(
                    wav.clone(),
                    "gsk-test",
                    None,
                    &hosted,
                    None,
                    None,
                ))
                .expect_err("transcription must be refused"),
            runtime
                .block_on(format_text("hi", "gsk-test", None, &hosted, None))
                .expect_err("cleanup must be refused"),
            runtime
                .block_on(request_speech(
                    "hi", "gsk-test", &hosted, "tts-1", "alloy", "mp3",
                ))
                .expect_err("voice must be refused"),
            runtime
                .block_on(fetch_models("gsk-test", &hosted))
                .expect_err("listing models must be refused"),
        ] {
            assert!(
                error.contains("Local only"),
                "every leg must name the setting: {error}"
            );
        }

        // Deepgram builds its URL by hand rather than through `Provider::endpoint`,
        // so it is checked separately or it would be the one way out.
        let deepgram_error = runtime
            .block_on(transcribe_audio(
                wav,
                "token",
                None,
                &Provider::Deepgram,
                None,
                None,
            ))
            .expect_err("Deepgram must be refused too");
        assert!(deepgram_error.contains("Local only"), "{deepgram_error}");

        // The positive control, and it has to be a real request: a guard that
        // refused everything would pass every assertion above. With Local only
        // still on, a loopback endpoint is contacted and answers.
        use std::io::{Read, Write};
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("bind a throwaway endpoint");
        let port = listener.local_addr().expect("local addr").port();
        let capture = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept the cleanup request");
            let mut buffer = vec![0u8; 4096];
            let read = stream.read(&mut buffer).unwrap_or(0);
            let body = br#"{"choices":[{"message":{"content":"hi"}}]}"#;
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                    body.len()
                )
                .as_bytes(),
            );
            let _ = stream.write_all(body);
            String::from_utf8_lossy(&buffer[..read]).into_owned()
        });
        let loopback = Provider::from_str(&format!("custom:http://127.0.0.1:{port}/v1"));
        assert_eq!(
            runtime.block_on(format_text("hi", "", None, &loopback, None)),
            Ok("hi".to_string()),
            "a loopback endpoint must still be reachable while Local only is on"
        );
        assert!(
            capture
                .join()
                .expect("capture thread")
                .starts_with("POST /v1/chat/completions "),
            "the request has to have crossed the socket"
        );
        set_local_only(false);
    }

    /// A loopback endpoint that answers 307 must not be able to send the audio
    /// off the machine.
    ///
    /// The URL check passes -- the endpoint the user configured really is
    /// loopback -- and then the client re-sends the request wherever the
    /// `Location` header says. That is the same leak the toggle exists to
    /// prevent, one hop later, so every hop is checked and not just the first.
    ///
    /// The hops here are cleanup calls, because those carry a JSON body reqwest
    /// can replay and therefore actually follows. The audio upload is a
    /// streaming multipart body, which this version of reqwest declines to
    /// replay across a redirect at all -- the last assertion pins that, since
    /// it is a second line of defence for the recording specifically and worth
    /// knowing if it ever changes.
    #[test]
    fn local_only_refuses_a_redirect_off_this_machine() {
        use std::io::{Read, Write};
        use std::sync::atomic::AtomicUsize;
        use std::sync::Arc;

        let _guard = LOCAL_ONLY_TESTS
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let runtime = tokio::runtime::Runtime::new().expect("test runtime");

        // Counts every connection, so a followed redirect shows up as a second
        // request rather than having to be inferred from an error string.
        fn serve(
            body: String,
            expected: usize,
            hits: Arc<AtomicUsize>,
        ) -> (u16, std::thread::JoinHandle<()>) {
            let listener =
                std::net::TcpListener::bind("127.0.0.1:0").expect("bind a throwaway endpoint");
            let port = listener.local_addr().expect("local addr").port();
            listener
                .set_nonblocking(true)
                .expect("poll rather than block forever");
            let handle = std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + Duration::from_secs(3);
                // Keep listening for a beat after the expected requests have
                // arrived, so an unwanted extra one is still counted rather
                // than refused by a socket that closed the moment it was
                // satisfied.
                let mut grace: Option<std::time::Instant> = None;
                loop {
                    if std::time::Instant::now() >= deadline {
                        return;
                    }
                    if let Some(since) = grace {
                        if since.elapsed() >= Duration::from_millis(250) {
                            return;
                        }
                    }
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let seen = hits.fetch_add(1, Ordering::SeqCst) + 1;
                            stream.set_nonblocking(false).ok();
                            stream
                                .set_read_timeout(Some(Duration::from_millis(500)))
                                .ok();
                            let mut buffer = vec![0u8; 8192];
                            let _ = stream.read(&mut buffer);
                            let _ = stream.write_all(body.as_bytes());
                            if seen >= expected {
                                grace = Some(std::time::Instant::now());
                            }
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(10)),
                    }
                }
            });
            (port, handle)
        }

        // 1. A loopback endpoint that redirects off the box, Local only on.
        let hits = Arc::new(AtomicUsize::new(0));
        let (port, server) = serve(
            "HTTP/1.1 307 Temporary Redirect\r\nLocation: https://example.com/v1/audio/transcriptions\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_string(),
            1,
            Arc::clone(&hits),
        );
        set_local_only(true);
        let refused = runtime
            .block_on(format_text(
                "a transcript",
                "",
                None,
                &Provider::from_str(&format!("custom:http://127.0.0.1:{port}/v1")),
                None,
            ))
            .expect_err("a redirect off this machine must not be followed");
        assert!(
            refused.contains("Local only"),
            "the refusal has to name the setting: {refused}"
        );
        assert!(
            refused.contains("example.com"),
            "and the host it refused to follow: {refused}"
        );
        let _ = server.join();
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "exactly one request: the redirect was refused, not followed"
        );

        // 2. The control. The same 307 with Local only off is followed to a
        // second listener -- without this, a policy that refused every redirect
        // would pass everything above.
        set_local_only(false);
        let target_hits = Arc::new(AtomicUsize::new(0));
        let answer = r#"{"choices":[{"message":{"content":"spoken"}}]}"#;
        let (target, target_server) = serve(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                answer.len(),
                answer
            ),
            1,
            Arc::clone(&target_hits),
        );
        let entry_hits = Arc::new(AtomicUsize::new(0));
        let (entry, entry_server) = serve(
            format!("HTTP/1.1 307 Temporary Redirect\r\nLocation: http://127.0.0.1:{target}/v1/chat/completions\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
            1,
            Arc::clone(&entry_hits),
        );
        let followed = runtime.block_on(format_text(
            "a transcript",
            "",
            None,
            &Provider::from_str(&format!("custom:http://127.0.0.1:{entry}/v1")),
            None,
        ));
        let _ = entry_server.join();
        let _ = target_server.join();
        assert_eq!(
            followed,
            Ok("spoken".to_string()),
            "an ordinary redirect still has to be followed"
        );
        assert_eq!(entry_hits.load(Ordering::SeqCst), 1);
        assert_eq!(target_hits.load(Ordering::SeqCst), 1);

        // 3. The recording itself, with Local only *off*, against a redirect it
        // could follow. It does not: a streaming multipart body cannot be
        // replayed, so the redirect comes back as the response instead. One
        // request, and the audio never left for the second host.
        let audio_hits = Arc::new(AtomicUsize::new(0));
        let (audio_entry, audio_server) = serve(
            "HTTP/1.1 307 Temporary Redirect\r\nLocation: https://example.com/v1/audio/transcriptions\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_string(),
            1,
            Arc::clone(&audio_hits),
        );
        let audio = runtime.block_on(transcribe_audio(
            vec![0u8; 64],
            "",
            None,
            &Provider::from_str(&format!("custom:http://127.0.0.1:{audio_entry}/v1")),
            None,
            None,
        ));
        let _ = audio_server.join();
        assert!(audio.is_err(), "a 307 is not a transcript");
        assert_eq!(
            audio_hits.load(Ordering::SeqCst),
            1,
            "the recording must not be re-sent to the redirect target"
        );
    }

    /// Everything that writes the process-wide flag takes turns, including
    /// `settings::tests`, which proves the setting arms this guard.
    pub(crate) static LOCAL_ONLY_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn explicit_output_modalities_override_generic_model_names() {
        let speech = serde_json::json!({
            "architecture": { "output_modalities": ["speech"] }
        });
        let transcription = serde_json::json!({
            "architecture": { "output_modalities": ["transcription"] }
        });
        assert_eq!(classify_model(&speech, "vendor/generic-audio"), Some("tts"));
        assert_eq!(
            classify_model(&transcription, "vendor/generic-audio"),
            Some("stt")
        );
    }
}
