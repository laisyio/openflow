//! Text to speech: which credential a request may carry, and the two ways a
//! host can ask for audio (download it whole, or stream it as it arrives).

use crate::engine::{EngineEvent, EngineEvents};
use crate::settings::Settings;
use crate::transcribe::{self, Provider};
use base64::Engine as _;
use futures_util::StreamExt;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

/// A speech response larger than this is refused rather than buffered.
const MAX_SPEECH_BYTES: u64 = 50 * 1024 * 1024;

/// What a host asks for. Every override is optional; blank falls back to the
/// saved setting, and an unset setting falls back to the provider's own name
/// for the thing.
#[derive(Clone, Debug, Default)]
pub struct SpeechRequest {
    pub text: String,
    pub model: Option<String>,
    pub voice: Option<String>,
    pub response_format: Option<String>,
    pub request_id: Option<String>,
}

#[derive(Serialize)]
pub struct SpeechAudio {
    pub data_base64: String,
    pub mime_type: String,
    pub format: String,
    pub model: String,
}

#[derive(Serialize, Clone)]
pub struct SpeechStarted {
    pub request_id: String,
    pub model: String,
    pub format: String,
}

#[derive(Serialize, Clone)]
pub struct SpeechChunk {
    pub request_id: String,
    pub sequence: u64,
    // Native hosts receive the bytes directly. Only a serializing host (the
    // Tauri bridge) pays for base64, retaining the existing webview wire format.
    #[serde(rename = "data_base64", serialize_with = "serialize_audio")]
    pub data: Vec<u8>,
}

fn serialize_audio<S: serde::Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&base64::engine::general_purpose::STANDARD.encode(bytes))
}

fn append_bounded(audio: &mut Vec<u8>, chunk: &[u8], limit: usize) -> Result<(), String> {
    if chunk.len() > limit.saturating_sub(audio.len()) {
        return Err("Generated speech is too large".to_string());
    }
    audio.extend_from_slice(chunk);
    Ok(())
}

#[derive(Serialize, Clone)]
pub struct SpeechResult {
    pub request_id: String,
    pub mime_type: String,
    pub format: String,
    pub model: String,
    pub bytes: u64,
}

#[derive(Serialize, Clone)]
pub struct SpeechError {
    pub request_id: String,
    pub error: String,
    pub cancelled: bool,
}

/// Resolve everything a speech call needs from the saved settings plus the
/// caller's overrides: which endpoint, which credential, and the model, voice
/// and container to ask for.
pub fn speech_settings(
    settings: &Settings,
    request: &SpeechRequest,
) -> Result<(Provider, String, String, String, String), String> {
    let provider = settings.tts_provider();
    let transcription_provider = settings.provider();
    let api_key = resolve_speech_key(
        &provider,
        settings.tts_api_key()?,
        settings.api_key()?,
        same_endpoint(&provider, &transcription_provider),
    )?;
    // Each provider has its own model and voice names, so blank falls back
    // per provider rather than to Gemini's.
    let model = request
        .model
        .clone()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| settings.tts_model())
        .unwrap_or_else(|| provider.default_tts_model().to_string());
    let voice = request
        .voice
        .clone()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| settings.tts_voice())
        .unwrap_or_else(|| provider.default_tts_voice().to_string());
    let format = request
        .response_format
        .clone()
        .filter(|value| !value.trim().is_empty())
        .map(|value| value.to_ascii_lowercase())
        .unwrap_or_else(|| settings.tts_response_format());
    Ok((provider, api_key, model, voice, format))
}

/// True when two provider settings name the same service, so one credential
/// is valid for both. Two custom endpoints are the same only if their URLs are.
pub fn same_endpoint(a: &Provider, b: &Provider) -> bool {
    match (a, b) {
        (Provider::Custom { base_url: x }, Provider::Custom { base_url: y }) => x == y,
        _ => std::mem::discriminant(a) == std::mem::discriminant(b),
    }
}

/// Which credential a speech request may carry.
///
/// The transcription key is shared only with the same endpoint. A different
/// hosted provider cannot use it (an OpenRouter key is worthless at OpenAI),
/// and a self-hosted server must not receive it: that would send a cloud
/// credential in clear text across the LAN. With no usable key, a custom
/// endpoint proceeds unauthenticated and a hosted one is refused up front.
pub fn resolve_speech_key(
    provider: &Provider,
    dedicated: Option<String>,
    shared: Option<String>,
    same_endpoint_as_transcription: bool,
) -> Result<String, String> {
    let present = |key: Option<String>| key.filter(|value| !value.trim().is_empty());
    if let Some(key) = present(dedicated) {
        return Ok(key);
    }
    if same_endpoint_as_transcription {
        if let Some(key) = present(shared) {
            return Ok(key);
        }
    }
    if provider.is_custom() {
        return Ok(String::new());
    }
    Err(if same_endpoint_as_transcription {
        "No API key set. Add your key in Settings.".to_string()
    } else {
        "The voice provider needs its own API key. Use the provider you transcribe with, or point the speech endpoint at a self-hosted server.".to_string()
    })
}

/// Download the whole clip, then hand it over base64-encoded.
pub async fn synthesize(
    settings: &Settings,
    request: &SpeechRequest,
) -> Result<SpeechAudio, String> {
    let (provider, api_key, model, voice, format) = speech_settings(settings, request)?;
    let response =
        transcribe::request_speech(&request.text, &api_key, &provider, &model, &voice, &format)
            .await?;
    if response
        .content_length()
        .is_some_and(|size| size > MAX_SPEECH_BYTES)
    {
        return Err("Generated speech is too large".to_string());
    }
    let mut bytes = Vec::new();
    let mut chunks = response.bytes_stream();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(|error| format!("Could not read speech audio: {}", error))?;
        append_bounded(&mut bytes, &chunk, MAX_SPEECH_BYTES as usize)?;
    }
    Ok(SpeechAudio {
        data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        mime_type: transcribe::speech_mime(&format).to_string(),
        format,
        model,
    })
}

/// Stream the clip out chunk by chunk through the event sink, so a host can
/// start playing before the provider has finished generating.
///
/// `jobs` holds the cancellation token under the request id for the life of the
/// call, which is what `cancel` reaches for.
pub async fn stream(
    settings: &Settings,
    events: &Arc<dyn EngineEvents>,
    jobs: &Mutex<HashMap<String, CancellationToken>>,
    request: SpeechRequest,
) -> Result<SpeechResult, String> {
    let request_id = request
        .request_id
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    if request_id.len() > 100
        || !request_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        return Err("Speech request id is invalid".to_string());
    }
    let (provider, api_key, model, voice, format) = speech_settings(settings, &request)?;
    let cancellation = CancellationToken::new();
    {
        let mut active = jobs
            .lock()
            .map_err(|_| "Speech job state is unavailable".to_string())?;
        if active.contains_key(&request_id) {
            return Err("A speech request with this id is already running".to_string());
        }
        active.insert(request_id.clone(), cancellation.clone());
    }
    let started = SpeechStarted {
        request_id: request_id.clone(),
        model: model.clone(),
        format: format.clone(),
    };
    let _ = events.emit(EngineEvent::TtsStarted(started));

    let text = request.text;
    let result = async {
        let response = tokio::select! {
            _ = cancellation.cancelled() => return Err("Speech generation cancelled".to_string()),
            response = transcribe::request_speech(&text, &api_key, &provider, &model, &voice, &format) => response?,
        };
        let mut stream = response.bytes_stream();
        let mut sequence = 0_u64;
        let mut total = 0_u64;
        loop {
            let next = tokio::select! {
                _ = cancellation.cancelled() => return Err("Speech generation cancelled".to_string()),
                next = stream.next() => next,
            };
            let Some(chunk) = next else { break; };
            let chunk = chunk.map_err(|error| format!("Speech stream failed: {}", error))?;
            total = total.saturating_add(chunk.len() as u64);
            if total > MAX_SPEECH_BYTES { return Err("Generated speech is too large".to_string()); }
            // A bounded native channel can now budget a slot as at most 64 KiB,
            // independent of how a provider or HTTP implementation frames data.
            for data in chunk.chunks(64 * 1024) {
                let payload = SpeechChunk {
                    request_id: request_id.clone(),
                    sequence,
                    data: data.to_vec(),
                };
                tokio::select! {
                    _ = cancellation.cancelled() => return Err("Speech generation cancelled".to_string()),
                    delivered = events.emit_speech_chunk(payload) => {
                        delivered.map_err(|error| format!("Could not deliver speech audio: {}", error))?;
                    }
                }
                sequence += 1;
            }
        }
        let result = SpeechResult {
            request_id: request_id.clone(),
            mime_type: transcribe::speech_mime(&format).to_string(),
            format: format.clone(),
            model: model.clone(),
            bytes: total,
        };
        let _ = events.emit(EngineEvent::TtsFinished(result.clone()));
        Ok(result)
    }.await;
    if let Err(error) = &result {
        let payload = SpeechError {
            request_id: request_id.clone(),
            error: error.clone(),
            cancelled: cancellation.is_cancelled(),
        };
        let _ = events.emit(EngineEvent::TtsError(payload));
    }
    if let Ok(mut active) = jobs.lock() {
        active.remove(&request_id);
    }
    result
}

/// Cancel one in-flight stream, or every one of them when no id is given.
pub fn cancel(
    jobs: &Mutex<HashMap<String, CancellationToken>>,
    request_id: Option<&str>,
) -> Result<bool, String> {
    let active = jobs
        .lock()
        .map_err(|_| "Speech job state is unavailable".to_string())?;
    let mut cancelled = false;
    for (id, token) in active.iter() {
        if request_id.map(|requested| requested == id).unwrap_or(true) {
            token.cancel();
            cancelled = true;
        }
    }
    Ok(cancelled)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_chunks_preserve_the_webview_wire_format() {
        let chunk = SpeechChunk {
            request_id: "a".into(),
            sequence: 2,
            data: vec![0, 1, 255],
        };
        let json = serde_json::to_value(&chunk).unwrap();
        assert_eq!(json["data_base64"], "AAH/");
        assert!(json.get("data").is_none());
        assert_eq!(chunk.data, [0, 1, 255]);
    }

    #[test]
    fn whole_speech_limit_is_checked_before_copying() {
        let mut audio = vec![1, 2];
        append_bounded(&mut audio, &[3, 4], 4).unwrap();
        let capacity = audio.capacity();
        assert!(append_bounded(&mut audio, &[5; 100], 4).is_err());
        assert_eq!(audio, [1, 2, 3, 4]);
        assert_eq!(audio.capacity(), capacity);
    }

    #[test]
    fn speech_key_never_leaks_across_endpoints() {
        let lan = Provider::from_str("custom:http://192.168.1.10:8880/v1");
        let shared = Some("sk-or-hosted".to_string());

        // A self-hosted server gets no key unless it has its own.
        assert_eq!(
            resolve_speech_key(&lan, None, shared.clone(), false),
            Ok(String::new())
        );
        assert_eq!(
            resolve_speech_key(&lan, Some("local".to_string()), shared.clone(), false),
            Ok("local".to_string())
        );
        // The same self-hosted server that transcribes may reuse its key.
        assert_eq!(
            resolve_speech_key(&lan, None, shared.clone(), true),
            Ok("sk-or-hosted".to_string())
        );
        // A hosted voice provider shares the key only with itself.
        assert_eq!(
            resolve_speech_key(&Provider::OpenRouter, None, shared.clone(), true),
            Ok("sk-or-hosted".to_string())
        );
        assert!(resolve_speech_key(&Provider::OpenAI, None, shared.clone(), false).is_err());
        assert!(resolve_speech_key(&Provider::OpenRouter, None, None, true).is_err());
        assert!(
            resolve_speech_key(&Provider::OpenRouter, Some("  ".to_string()), None, true).is_err()
        );
    }

    /// The unit tests above prove `resolve_speech_key` *returns* no key for a
    /// self-hosted endpoint. They do not prove the HTTP request that goes out
    /// carries no key -- a header could still be added downstream, and a LAN
    /// server that accepts any credential would answer 200 either way.
    ///
    /// So stand up a listener, send a real request at it, and read the bytes
    /// that actually crossed the socket.
    #[test]
    fn selfhosted_request_carries_no_cloud_credential() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a throwaway endpoint");
        let port = listener.local_addr().expect("local addr").port();

        let capture = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept the speech request");
            let mut buf = vec![0u8; 8192];
            let read = stream.read(&mut buf).unwrap_or(0);
            // Non-empty: `validate_speech_response` rejects a zero-length body.
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nContent-Length: 4\r\n\r\nID3\x04",
            );
            String::from_utf8_lossy(&buf[..read]).into_owned()
        });

        let lan = Provider::from_str(&format!("custom:http://127.0.0.1:{port}/v1"));
        let cloud_key = "sk-or-this-must-never-reach-the-lan";

        // Exactly what the speech command computes before it sends: a shared
        // transcription key exists, but the speech endpoint is a different one.
        let key = resolve_speech_key(&lan, None, Some(cloud_key.to_string()), false)
            .expect("a custom endpoint proceeds unauthenticated");

        let sent = tokio::runtime::Runtime::new()
            .expect("test runtime")
            .block_on(transcribe::request_speech(
                "probe", &key, &lan, "tts-1", "alloy", "mp3",
            ));
        assert!(
            sent.is_ok(),
            "the self-hosted request should go through: {:?}",
            sent.err()
        );

        let raw = capture.join().expect("capture thread");

        // reqwest lowercases header names, so match case-insensitively: the
        // point is that no credential header exists at all, not that one
        // exists carrying an empty value.
        let headers = raw.to_ascii_lowercase();
        assert!(
            !headers.contains("authorization"),
            "a self-hosted endpoint must receive no Authorization header, got:\n{raw}"
        );
        assert!(
            !raw.contains(cloud_key),
            "the transcription key leaked to the LAN, got:\n{raw}"
        );

        // Pin the positive contract too, so a future failure separates "leaked"
        // from "never reached the endpoint".
        assert!(
            raw.starts_with("POST /v1/audio/speech "),
            "expected a speech request, got:\n{raw}"
        );
        assert!(
            raw.contains(r#""model":"tts-1""#) && raw.contains(r#""voice":"alloy""#),
            "expected model and voice in the body, got:\n{raw}"
        );
    }

    #[test]
    fn same_endpoint_compares_custom_urls_and_hosted_variants() {
        let a = Provider::from_str("custom:http://10.0.0.5:8880/v1");
        let b = Provider::from_str("custom:http://10.0.0.5:8880/v1/");
        let c = Provider::from_str("custom:http://10.0.0.9:8880/v1");
        assert!(same_endpoint(&a, &b));
        assert!(!same_endpoint(&a, &c));
        assert!(same_endpoint(&Provider::OpenRouter, &Provider::OpenRouter));
        assert!(!same_endpoint(&Provider::OpenRouter, &Provider::OpenAI));
        assert!(!same_endpoint(&Provider::OpenRouter, &a));
    }
}
