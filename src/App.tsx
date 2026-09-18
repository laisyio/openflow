import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-shell";
import { SpeechPlayback, type SpeechChunk, type SpeechCompletion } from "./ttsPlayback";

type Screen = "onboarding" | "main" | "history" | "settings" | "plugins";
type RecordingState = "idle" | "recording" | "transcribing";
type OnboardingStep = "provider" | "credentials" | "models";
type ConnectionState = "idle" | "checking" | "connected" | "error";
type ModelTarget = "transcription" | "formatting" | "tts";

interface Transcription {
  id: string;
  raw_text: string;
  formatted_text: string | null;
  provider: string;
  duration_ms: number | null;
  context_type: string | null;
  window_title: string | null;
  language: string | null;
  created_at: string;
}

interface PluginInfo {
  manifest: {
    id: string;
    name: string;
    version: string;
    description: string;
    author: string | null;
    hooks: string[];
  };
  enabled: boolean;
  path: string;
}

interface ModelInfo {
  id: string;
  name: string;
  model_type: "stt" | "chat" | "tts" | string;
}

interface AudioDevice {
  id: string;
  name: string;
  is_default: boolean;
}

interface TtsStreamResult {
  request_id: string;
  mime_type: string;
  format: string;
  model: string;
  bytes: number;
}

interface ProviderDefinition {
  label: string;
  description: string;
  keyUrl: string;
  keyPlaceholder: string;
  sttDefault: string;
  chatDefault: string;
  recommended?: boolean;
}

const PROVIDERS: Record<string, ProviderDefinition> = {
  groq: {
    label: "Groq",
    description: "Fastest Whisper and cleanup. One key, no proxy hop.",
    keyUrl: "https://console.groq.com/keys",
    keyPlaceholder: "gsk_…",
    sttDefault: "whisper-large-v3-turbo",
    chatDefault: "openai/gpt-oss-20b",
    recommended: true,
  },
  openrouter: {
    label: "OpenRouter",
    description: "One key for many models, plus Gemini voice.",
    keyUrl: "https://openrouter.ai/keys",
    keyPlaceholder: "sk-or-v1-…",
    sttDefault: "openai/whisper-1",
    chatDefault: "google/gemini-3.1-flash-lite-preview",
  },
  openai: {
    label: "OpenAI",
    description: "Reliable speech-to-text and compact formatting models.",
    keyUrl: "https://platform.openai.com/api-keys",
    keyPlaceholder: "sk-…",
    sttDefault: "whisper-1",
    chatDefault: "gpt-4o-mini",
  },
  deepgram: {
    label: "Deepgram",
    description: "Nova speech recognition with broad language coverage.",
    keyUrl: "https://console.deepgram.com",
    keyPlaceholder: "Paste your Deepgram key",
    sttDefault: "nova-3",
    chatDefault: "openai/gpt-oss-20b",
  },
  custom: {
    label: "Custom endpoint",
    description: "Connect any OpenAI-compatible speech or chat service.",
    keyUrl: "",
    keyPlaceholder: "API key",
    sttDefault: "whisper-large-v3",
    chatDefault: "default",
  },
};

const TTS_DEFAULT_MODEL = "google/gemini-3.1-flash-tts-preview";
// Groq's Orpheus answers only in WAV (mp3 is a 400), so the format is a
// property of the provider. WAV cannot be fed to a MediaSource, so those
// providers play back after the download completes instead of mid-stream.
const TTS_DEFAULTS: Record<string, { model: string; voice: string; voices: string[]; format: "mp3" | "wav" }> = {
  groq: { model: "canopylabs/orpheus-v1-english", voice: "troy", voices: ["troy", "hannah", "austin", "autumn", "daniel", "diana"], format: "wav" },
  openrouter: { model: TTS_DEFAULT_MODEL, voice: "Kore", voices: ["Kore", "Aoede", "Puck", "Charon", "Fenrir", "Leda", "Orus", "Zephyr"], format: "mp3" },
  openai: { model: "tts-1", voice: "alloy", voices: ["alloy", "echo", "fable", "onyx", "nova", "shimmer"], format: "mp3" },
  custom: { model: "kokoro", voice: "af_bella", voices: [], format: "mp3" },
};

function ttsFormat(provider: string): "mp3" | "wav" {
  return TTS_DEFAULTS[provider]?.format ?? "mp3";
}
const LANGUAGE_OPTIONS = [
  ["auto", "Auto-detect"], ["en", "English"], ["es", "Spanish"], ["fr", "French"],
  ["de", "German"], ["it", "Italian"], ["pt", "Portuguese"], ["nl", "Dutch"],
  ["ja", "Japanese"], ["ko", "Korean"], ["zh", "Chinese"], ["ar", "Arabic"],
  ["hi", "Hindi"], ["ru", "Russian"],
] as const;

const STEP_ORDER: OnboardingStep[] = ["provider", "credentials", "models"];

function isTauriRuntime() {
  return "__TAURI_INTERNALS__" in window;
}

function initialTheme(): "dark" | "light" {
  try {
    const stored = window.localStorage.getItem("openflow-theme");
    if (stored === "dark" || stored === "light") return stored;
  } catch { /* Local storage can be unavailable in hardened webviews. */ }
  return window.matchMedia?.("(prefers-color-scheme: dark)").matches ? "dark" : "light";
}

function friendlyError(error: unknown) {
  const message = error instanceof Error ? error.message : String(error);
  return message.replace(/^Error:\s*/i, "").replace(/^\"|\"$/g, "");
}

function parseStoredProvider(value: string | null) {
  if (!value) return { name: "groq", customUrl: "" };
  if (value.startsWith("custom:")) return { name: "custom", customUrl: value.slice(7) };
  return { name: PROVIDERS[value] ? value : "groq", customUrl: "" };
}

function providerValue(name: string, customUrl: string) {
  return name === "custom" ? `custom:${customUrl.trim().replace(/\/+$/, "")}` : name;
}

function Icon({ name, size = 18 }: { name: "arrow" | "check" | "clock" | "gear" | "mic" | "play" | "refresh" | "spark" | "stop" | "volume"; size?: number }) {
  const paths: Record<typeof name, ReactNode> = {
    arrow: <><path d="m15 18-6-6 6-6"/><path d="M9 12h10"/></>,
    check: <path d="m5 12 4 4L19 6"/>,
    clock: <><circle cx="12" cy="12" r="9"/><path d="M12 7v5l3 2"/></>,
    gear: <><circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.7 1.7 0 0 0 .34 1.88l.06.06-2.86 2.86-.06-.06A1.7 1.7 0 0 0 15 19.4a1.7 1.7 0 0 0-1 .6 1.7 1.7 0 0 0-.4 1.1V21H9.55v-.1A1.7 1.7 0 0 0 8.5 19.4a1.7 1.7 0 0 0-1.88.34l-.06.06-2.86-2.86.06-.06A1.7 1.7 0 0 0 4.1 15a1.7 1.7 0 0 0-.6-1 1.7 1.7 0 0 0-1.1-.4H2.3V9.55h.1A1.7 1.7 0 0 0 4.1 8.5a1.7 1.7 0 0 0-.34-1.88l-.06-.06L6.56 3.7l.06.06A1.7 1.7 0 0 0 8.5 4.1a1.7 1.7 0 0 0 1-.6 1.7 1.7 0 0 0 .4-1.1v-.1h4.05v.1A1.7 1.7 0 0 0 15 4.1a1.7 1.7 0 0 0 1.88-.34l.06-.06 2.86 2.86-.06.06A1.7 1.7 0 0 0 19.4 8.5c.18.4.5.74.9.95.25.13.53.2.8.2h.1v4.05h-.1a1.7 1.7 0 0 0-1.7 1.3Z"/></>,
    mic: <><rect x="9" y="3" width="6" height="11" rx="3"/><path d="M5 11a7 7 0 0 0 14 0M12 18v3M9 21h6"/></>,
    play: <path d="m9 7 8 5-8 5Z"/>,
    refresh: <><path d="M20 7v5h-5"/><path d="M4 17v-5h5"/><path d="M6.1 8a7 7 0 0 1 11.6-2L20 8M4 16l2.3 2a7 7 0 0 0 11.6-2"/></>,
    spark: <><path d="m12 3 1.2 3.8L17 8l-3.8 1.2L12 13l-1.2-3.8L7 8l3.8-1.2Z"/><path d="m18 14 .7 2.3L21 17l-2.3.7L18 20l-.7-2.3L15 17l2.3-.7Z"/></>,
    stop: <rect x="7" y="7" width="10" height="10" rx="1"/>,
    volume: <><path d="M11 5 6 9H3v6h3l5 4Z"/><path d="M15 9a4 4 0 0 1 0 6M17.7 6.3a8 8 0 0 1 0 11.4"/></>,
  };
  return <svg aria-hidden="true" width={size} height={size} viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round">{paths[name]}</svg>;
}

function Toggle({ checked, onChange, label, disabled = false }: { checked: boolean; onChange: (checked: boolean) => void; label: string; disabled?: boolean }) {
  return (
    <label className="switch-control">
      <span className="sr-only">{label}</span>
      <input type="checkbox" checked={checked} disabled={disabled} onChange={(event) => onChange(event.target.checked)} />
      <span className="switch-track" aria-hidden="true"><span /></span>
    </label>
  );
}

function App() {
  const [booting, setBooting] = useState(true);
  const [screen, setScreen] = useState<Screen>("onboarding");
  const [onboardingStep, setOnboardingStep] = useState<OnboardingStep>("provider");
  const [status, setStatus] = useState<RecordingState>("idle");
  const [lastTranscription, setLastTranscription] = useState("");
  const [error, setError] = useState("");
  const [notification, setNotification] = useState("");
  const [history, setHistory] = useState<Transcription[]>([]);
  const [searchQuery, setSearchQuery] = useState("");
  const [plugins, setPlugins] = useState<PluginInfo[]>([]);

  const [transcriptionProvider, setTranscriptionProvider] = useState("groq");
  const [transcriptionKey, setTranscriptionKey] = useState("");
  const [transcriptionModel, setTranscriptionModel] = useState(PROVIDERS.groq.sttDefault);
  const [formattingProvider, setFormattingProvider] = useState("groq");
  const [formattingKey, setFormattingKey] = useState("");
  const [formattingModel, setFormattingModel] = useState(PROVIDERS.groq.chatDefault);
  const [sameProvider, setSameProvider] = useState(true);
  const [language, setLanguage] = useState("auto");
  const [theme, setTheme] = useState<"dark" | "light">(initialTheme);
  const [formatEnabled, setFormatEnabled] = useState(true);
  const [customTranscriptionUrl, setCustomTranscriptionUrl] = useState("");
  const [customFormattingUrl, setCustomFormattingUrl] = useState("");
  const [microphone, setMicrophone] = useState("");
  const [microphones, setMicrophones] = useState<AudioDevice[]>([]);
  const [microphonesLoading, setMicrophonesLoading] = useState(false);
  const [transcriptionModels, setTranscriptionModels] = useState<ModelInfo[]>([]);
  const [formattingModels, setFormattingModels] = useState<ModelInfo[]>([]);
  const [ttsModels, setTtsModels] = useState<ModelInfo[]>([]);
  const [modelsLoading, setModelsLoading] = useState<ModelTarget | null>(null);
  const [connectionState, setConnectionState] = useState<ConnectionState>("idle");
  const [connectionMessage, setConnectionMessage] = useState("");
  const [recordHotkey, setRecordHotkey] = useState("Option+V");
  const [recopyHotkey, setRecopyHotkey] = useState("Ctrl+Shift+V");
  const [hotkeyDraft, setHotkeyDraft] = useState("");
  const [editingHotkey, setEditingHotkey] = useState<null | "record" | "recopy">(null);
  const [showRecents, setShowRecents] = useState(false);
  const [showTranscriptionKey, setShowTranscriptionKey] = useState(false);
  const [showFormattingKey, setShowFormattingKey] = useState(false);
  const [savingSettings, setSavingSettings] = useState(false);
  const [settingsDirty, setSettingsDirty] = useState(false);
  const [saveHistory, setSaveHistory] = useState(true);
  const [insertMethod, setInsertMethod] = useState("paste");
  const [preserveClipboard, setPreserveClipboard] = useState(true);
  const [overlayOnlyWhileRecording, setOverlayOnlyWhileRecording] = useState(false);
  const [retentionDays, setRetentionDays] = useState("");
  const [dictionary, setDictionary] = useState("");
  const [confirmingClear, setConfirmingClear] = useState(false);

  const [ttsEnabled, setTtsEnabled] = useState(true);
  const [ttsModel, setTtsModel] = useState(TTS_DEFAULT_MODEL);
  const [ttsVoice, setTtsVoice] = useState("Kore");
  const [ttsProvider, setTtsProvider] = useState("groq");
  const [customTtsUrl, setCustomTtsUrl] = useState("");
  const [ttsPreviewText, setTtsPreviewText] = useState("OpenFlow is ready. Your ideas can move at the speed of your voice.");
  const [ttsStatus, setTtsStatus] = useState<"idle" | "streaming" | "ready" | "error">("idle");
  const [ttsError, setTtsError] = useState("");
  const [ttsAudioUrl, setTtsAudioUrl] = useState("");
  const ttsPlayerRef = useRef<SpeechPlayback | null>(null);
  const audioRef = useRef<HTMLAudioElement | null>(null);
  if (!ttsPlayerRef.current) {
    ttsPlayerRef.current = new SpeechPlayback({
      audio: () => audioRef.current,
      url: setTtsAudioUrl, state: setTtsStatus, error: setTtsError,
      cancel: requestId => { void invoke("cancel_speech", { requestId }).catch(() => {}); },
    });
  }
  const cancelHotkeyEditRef = useRef(false);

  // Copy without the paste keystroke. paste_text is the other verb, used by the
  // tray and the re-copy hotkey where focus is in the user's editor.
  const copyToClipboard = async (text: string) => {
    try { await invoke("copy_text", { text }); showNotification("Copied to clipboard"); }
    catch (err) { setError(String(err)); }
  };

  const handleDeleteTranscription = async (id: string) => {
    try {
      await invoke("delete_transcription", { id });
      setHistory((prev) => prev.filter((entry) => entry.id !== id));
      showNotification("Deleted");
    } catch (err) { setError(String(err)); }
  };

  const handleClearHistory = async () => {
    if (!confirmingClear) { setConfirmingClear(true); return; }
    try {
      const removed = await invoke<number>("clear_history");
      setHistory([]);
      showNotification(`Deleted ${removed} transcription${removed === 1 ? "" : "s"}`);
    } catch (err) { setError(String(err)); }
    setConfirmingClear(false);
  };

  const showNotification = useCallback((message: string) => {
    setNotification(message);
    window.setTimeout(() => setNotification(""), 2400);
  }, []);

  const loadHistory = useCallback(async () => {
    try { setHistory(await invoke<Transcription[]>("get_history", { limit: 50 })); } catch { /* App may be previewed outside Tauri. */ }
  }, []);

  const loadPlugins = useCallback(async () => {
    try { setPlugins(await invoke<PluginInfo[]>("list_plugins")); } catch (reason) { setError(friendlyError(reason)); }
  }, []);

  const loadMicrophones = useCallback(async () => {
    setMicrophonesLoading(true);
    try {
      const devices = await invoke<AudioDevice[]>("list_audio_devices");
      setMicrophones(devices);
      setMicrophone((current) => {
        if (!current || devices.some((device) => device.id === current)) return current;
        // Migrate selections saved by older versions, which used the display
        // name as the id and could not distinguish duplicate device names.
        return devices.find((device) => device.name === current)?.id || "";
      });
    } catch (reason) {
      setError(`Could not read microphones. ${friendlyError(reason)}`);
    } finally {
      setMicrophonesLoading(false);
    }
  }, []);

  useEffect(() => {
    let mounted = true;
    void (async () => {
      if (!isTauriRuntime()) {
        setBooting(false);
        return;
      }
      try {
        const secretReads = Promise.allSettled([
          invoke<string | null>("get_api_key"),
          invoke<string | null>("get_setting", { key: "formatting_api_key" }),
        ]);
        const [storedProvider, storedFormattingProvider, storedSameProvider, storedLanguage, storedTheme, storedFormatEnabled, storedSttModel, storedChatModel, storedMicrophone, storedRecordHotkey, storedRecopyHotkey, storedTtsEnabled, storedTtsModel, storedTtsVoice, storedTtsProvider, storedSaveHistory, storedRetentionDays, storedDictionary, storedInsertMethod, storedOverlayOnly, storedPreserveClipboard] = await Promise.all([
          invoke<string | null>("get_setting", { key: "provider" }),
          invoke<string | null>("get_setting", { key: "formatting_provider" }),
          invoke<string | null>("get_setting", { key: "same_provider" }),
          invoke<string | null>("get_setting", { key: "language" }),
          invoke<string | null>("get_setting", { key: "theme" }),
          invoke<string | null>("get_setting", { key: "format_enabled" }),
          invoke<string | null>("get_setting", { key: "stt_model" }),
          invoke<string | null>("get_setting", { key: "chat_model" }),
          invoke<string | null>("get_setting", { key: "microphone" }),
          invoke<string | null>("get_setting", { key: "hotkey_record" }),
          invoke<string | null>("get_setting", { key: "hotkey_recopy" }),
          invoke<string | null>("get_setting", { key: "tts_enabled" }),
          invoke<string | null>("get_setting", { key: "tts_model" }),
          invoke<string | null>("get_setting", { key: "tts_voice" }),
          invoke<string | null>("get_setting", { key: "tts_provider" }),
          invoke<string | null>("get_setting", { key: "save_history" }),
          invoke<string | null>("get_setting", { key: "history_retention_days" }),
          invoke<string | null>("get_setting", { key: "dictionary" }),
          invoke<string | null>("get_setting", { key: "insert_method" }),
          invoke<string | null>("get_setting", { key: "overlay_only_while_recording" }),
          invoke<string | null>("get_setting", { key: "preserve_clipboard" }),
        ]);
        const [keyResult, formattingKeyResult] = await secretReads;
        if (!mounted) return;
        const key = keyResult.status === "fulfilled" ? keyResult.value : null;
        const storedFormattingKey = formattingKeyResult.status === "fulfilled" ? formattingKeyResult.value : null;
        const secretFailure = keyResult.status === "rejected"
          ? keyResult.reason
          : formattingKeyResult.status === "rejected"
            ? formattingKeyResult.reason
            : null;
        if (secretFailure) {
          setError(`Protected credentials could not be read. ${friendlyError(secretFailure)}`);
        }
        const transcription = parseStoredProvider(storedProvider);
        let formatting = parseStoredProvider(storedFormattingProvider || storedProvider);
        if (formatting.name === "deepgram") {
          formatting = { name: "groq", customUrl: "" };
        }
        setTranscriptionProvider(transcription.name);
        setCustomTranscriptionUrl(transcription.customUrl);
        setFormattingProvider(formatting.name);
        setCustomFormattingUrl(formatting.customUrl);
        if (key) {
          setTranscriptionKey(key);
          setFormattingKey(storedFormattingKey || key);
        }
        // A self-hosted transcription endpoint may legitimately have no key,
        // so a saved provider is what marks setup as complete.
        if (key || storedProvider) setScreen("main");
        setSameProvider(transcription.name === "deepgram" ? false : storedSameProvider !== "false");
        if (storedLanguage) setLanguage(storedLanguage);
        if (storedTheme === "dark" || storedTheme === "light") setTheme(storedTheme);
        setFormatEnabled(storedFormatEnabled !== "false");
        if (storedSttModel) setTranscriptionModel(storedSttModel);
        if (storedChatModel) setFormattingModel(storedChatModel);
        if (storedMicrophone) setMicrophone(storedMicrophone);
        if (storedRecordHotkey) setRecordHotkey(storedRecordHotkey);
        if (storedRecopyHotkey) setRecopyHotkey(storedRecopyHotkey);
        setTtsEnabled(storedTtsEnabled !== "false");
        if (storedTtsModel) setTtsModel(storedTtsModel);
        if (storedTtsVoice) setTtsVoice(storedTtsVoice);
        const parsedTts = parseStoredProvider(storedTtsProvider);
        setTtsProvider(parsedTts.name);
        setCustomTtsUrl(parsedTts.customUrl);
        if (storedSaveHistory === "false") setSaveHistory(false);
        if (storedRetentionDays) setRetentionDays(storedRetentionDays);
        if (storedDictionary) setDictionary(storedDictionary);
        if (storedInsertMethod === "type" || storedInsertMethod === "paste") setInsertMethod(storedInsertMethod);
        setOverlayOnlyWhileRecording(storedOverlayOnly === "true");
        setPreserveClipboard(storedPreserveClipboard !== "false");
      } catch (reason) {
        if (mounted) setError(`OpenFlow settings could not be loaded. ${friendlyError(reason)}`);
      } finally {
        if (mounted) setBooting(false);
      }
      void loadHistory();
      void loadMicrophones();
    })();
    return () => { mounted = false; };
  }, [loadHistory, loadMicrophones]);

  useEffect(() => {
    document.documentElement.dataset.theme = theme;
    document.documentElement.style.colorScheme = theme;
    try { window.localStorage.setItem("openflow-theme", theme); } catch { /* Best effort. */ }
  }, [theme]);

  useEffect(() => {
    // Keep the Vite/browser preview usable for visual QA. Tauri's event bridge
    // only exists inside the packaged desktop webview.
    if (!isTauriRuntime()) return;

    const unlisteners = [
      listen<string>("recording-state", (event) => setStatus(event.payload as RecordingState)),
      listen<Transcription>("transcription-result", (event) => {
        const transcription = event.payload;
        setLastTranscription(transcription.formatted_text || transcription.raw_text);
        setHistory((current) => [transcription, ...current.filter((item) => item.id !== transcription.id)].slice(0, 50));
        setError("");
      }),
      listen<string>("transcription-error", (event) => {
        if (event.payload === "Transcription cancelled") {
          setError("");
          showNotification("Transcription cancelled");
        } else {
          setError(event.payload);
        }
      }),
      listen<string>("transcription-warning", (event) => showNotification(event.payload)),
      listen<string>("recopy-success", (event) => showNotification(event.payload)),
      listen<string>("navigate", (event) => {
        if (event.payload === "history") { void loadHistory(); setScreen("history"); }
      }),
      listen<SpeechChunk>("tts-audio-chunk", event => ttsPlayerRef.current?.chunk(event.payload)),
      listen<SpeechCompletion>("tts-finished", event => ttsPlayerRef.current?.finish(event.payload)),
      listen<{ request_id: string; error: string }>("tts-error", event => {
        ttsPlayerRef.current?.fail(event.payload.request_id, event.payload.error);
      }),
    ];
    return () => { for (const unlisten of unlisteners) void unlisten.then((dispose) => dispose()); };
  }, [loadHistory, showNotification]);

  useEffect(() => () => ttsPlayerRef.current?.dispose(), []);
  useEffect(() => {
    // A completed buffered/WAV preview creates a new URL. Streaming MP3 keeps
    // its existing URL, so completion never resumes a manually paused stream.
    if (ttsAudioUrl && ttsStatus === "ready") {
      const url = ttsAudioUrl;
      void audioRef.current?.play().catch(() => {
        if (audioRef.current?.getAttribute("src") === url) {
          setTtsError("Playback was blocked. Use the audio controls to continue.");
        }
      });
    }
  }, [ttsAudioUrl]);
  // Editing voice settings invalidates the preview and its pending result.
  useEffect(() => { ttsPlayerRef.current?.dispose(); },
    [ttsProvider, customTtsUrl, ttsModel, ttsVoice, ttsPreviewText, ttsEnabled]);

  const markDirty = () => {
    setSettingsDirty(true);
    setConnectionState("idle");
    setConnectionMessage("");
  };

  const setProvider = (target: "transcription" | "formatting", nextProvider: string) => {
    const defaults = PROVIDERS[nextProvider];
    if (target === "transcription") {
      setTranscriptionProvider(nextProvider);
      setTranscriptionModel(defaults.sttDefault);
      if (nextProvider === "deepgram") {
        setSameProvider(false);
        setFormattingProvider("groq");
        setFormattingModel(PROVIDERS.groq.chatDefault);
        setFormattingKey("");
      } else if (sameProvider) {
        setFormattingProvider(nextProvider);
        setFormattingModel(defaults.chatDefault);
      }
    } else {
      setFormattingProvider(nextProvider);
      setFormattingModel(defaults.chatDefault);
    }
    markDirty();
  };

  const validateProviderConfiguration = (providerName: string, customUrl: string, apiKey: string) => {
    if (!apiKey.trim() && providerName !== "custom") return "Enter an API key to continue.";
    if (providerName === "custom" && !/^https?:\/\//i.test(customUrl.trim())) return "Enter a complete endpoint URL beginning with http:// or https://.";
    return "";
  };

  const loadModelsFor = async (providerName: string, customUrl: string, apiKey: string, target: ModelTarget) => {
    const validationError = validateProviderConfiguration(providerName, customUrl, apiKey);
    if (validationError) throw new Error(validationError);
    setModelsLoading(target);
    try {
      const models = await invoke<ModelInfo[]>("fetch_models", {
        providerName: providerValue(providerName, customUrl),
        apiKeyOverride: apiKey.trim(),
      });
      if (target === "transcription") {
        setTranscriptionModels(models.filter((model) => model.model_type === "stt"));
        setTtsModels(models.filter((model) => model.model_type === "tts"));
      } else if (target === "formatting") {
        setFormattingModels(models.filter((model) => model.model_type === "chat"));
      } else {
        setTtsModels(models.filter((model) => model.model_type === "tts"));
      }
      return models;
    } finally {
      setModelsLoading(null);
    }
  };

  const verifyConnection = async () => {
    setConnectionState("checking");
    setConnectionMessage("");
    setError("");
    try {
      const primaryModels = await loadModelsFor(transcriptionProvider, customTranscriptionUrl, transcriptionKey, "transcription");
      if (sameProvider) {
        setFormattingModels(primaryModels.filter((model) => model.model_type === "chat"));
        setTtsModels(primaryModels.filter((model) => model.model_type === "tts"));
      } else {
        await loadModelsFor(formattingProvider, customFormattingUrl, formattingKey, "formatting");
      }
      setConnectionState("connected");
      setConnectionMessage(`Connected to ${PROVIDERS[transcriptionProvider].label}. Your key is valid and model access is ready.`);
    } catch (reason) {
      setConnectionState("error");
      setConnectionMessage(`Connection failed. ${friendlyError(reason)}`);
    }
  };

  const saveConfiguration = async () => {
    const validationError = validateProviderConfiguration(transcriptionProvider, customTranscriptionUrl, transcriptionKey);
    if (validationError) throw new Error(validationError);
    if (!sameProvider) {
      const formattingValidationError = validateProviderConfiguration(formattingProvider, customFormattingUrl, formattingKey);
      if (formattingValidationError) throw new Error(formattingValidationError);
    }
    const settings: Array<[string, string]> = [
      ["provider", providerValue(transcriptionProvider, customTranscriptionUrl)],
      ["same_provider", String(sameProvider)],
      ["format_enabled", String(formatEnabled)],
      ["stt_model", transcriptionModel.trim()],
      ["chat_model", formattingModel.trim()],
      ["language", language === "auto" ? "" : language],
      ["theme", theme],
      ["microphone", microphone],
      ["tts_enabled", String(ttsEnabled)],
      ["tts_provider", providerValue(ttsProvider, customTtsUrl)],
      ["tts_model", ttsModel.trim()],
      ["tts_voice", ttsVoice.trim()],
      ["tts_response_format", ttsFormat(ttsProvider)],
      ["save_history", String(saveHistory)],
      ["history_retention_days", retentionDays],
      ["dictionary", dictionary.trim()],
      ["insert_method", insertMethod],
      ["overlay_only_while_recording", String(overlayOnlyWhileRecording)],
      ["preserve_clipboard", String(preserveClipboard)],
    ];
    if (!sameProvider) {
      settings.push(
        ["formatting_provider", providerValue(formattingProvider, customFormattingUrl)],
        ["formatting_api_key", formattingKey.trim()],
      );
    }
    await invoke("set_api_key", { key: transcriptionKey.trim() });
    await Promise.all(settings.map(([key, value]) => invoke("set_setting", { key, value })));
  };

  const finishOnboarding = async () => {
    setSavingSettings(true);
    setError("");
    try {
      await saveConfiguration();
      setSettingsDirty(false);
      setScreen("main");
      showNotification("OpenFlow is ready");
    } catch (reason) {
      setError(`Setup could not be saved. ${friendlyError(reason)}`);
    } finally {
      setSavingSettings(false);
    }
  };

  const saveSettings = async () => {
    setSavingSettings(true);
    setError("");
    try {
      await saveConfiguration();
      setSettingsDirty(false);
      showNotification("Settings saved");
    } catch (reason) {
      setError(`Settings could not be saved. ${friendlyError(reason)}`);
    } finally {
      setSavingSettings(false);
    }
  };

  const handleSearch = async () => {
    if (!searchQuery.trim()) { void loadHistory(); return; }
    try { setHistory(await invoke<Transcription[]>("search_history", { query: searchQuery.trim() })); }
    catch (reason) { setError(`Search failed. ${friendlyError(reason)}`); }
  };

  const handleStartRecording = async () => {
    if (status !== "idle") return;
    setError("");
    setStatus("recording");
    try { await invoke("start_recording"); }
    catch (reason) { setError(`Recording could not start. ${friendlyError(reason)}`); setStatus("idle"); }
  };

  const handleStopRecording = async () => {
    if (status !== "recording") return;
    setStatus("transcribing");
    try {
      const result = await invoke<Transcription>("stop_recording_and_transcribe");
      setLastTranscription(result.formatted_text || result.raw_text);
      setHistory((current) => [result, ...current.filter((item) => item.id !== result.id)].slice(0, 50));
      setStatus("idle");
      setError("");
    } catch (reason) {
      const message = friendlyError(reason);
      if (message === "Transcription cancelled") {
        setError("");
        showNotification("Transcription cancelled");
      } else {
        setError(`Transcription failed. ${message}`);
      }
      setStatus("idle");
    }
  };

  const cancelTranscription = async () => {
    try {
      const cancelled = await invoke<boolean>("cancel_current_transcription");
      if (cancelled) showNotification("Cancelling transcription…");
    } catch (reason) {
      setError(`Transcription could not be cancelled. ${friendlyError(reason)}`);
    }
  };

  const playTtsPreview = async () => {
    if (!ttsPreviewText.trim()) { setTtsError("Enter a short preview sentence first."); return; }
    if (ttsProvider !== "custom" && transcriptionProvider !== ttsProvider) { setTtsError(`${PROVIDERS[ttsProvider]?.label ?? ttsProvider} voice previews reuse your transcription key, so choose the same provider for transcription or use a self-hosted endpoint.`); return; }
    if (ttsStatus === "ready" && ttsAudioUrl) {
      try {
        if (audioRef.current) audioRef.current.currentTime = 0;
        await audioRef.current?.play();
      } catch { setTtsError("Playback was blocked. Use the audio controls below to play the preview."); }
      return;
    }
    const requestId = crypto.randomUUID();
    const format = ttsFormat(ttsProvider);
    const player = ttsPlayerRef.current!;
    player.start(requestId, format === "wav" ? "audio/wav" : "audio/mpeg");
    try {
      const result = await invoke<TtsStreamResult>("stream_speech", {
        text: ttsPreviewText.trim(), model: ttsModel.trim(), voice: ttsVoice.trim(),
        responseFormat: format, requestId,
      });
      player.finish(result);
    } catch (reason) {
      player.fail(requestId, `Voice preview failed. ${friendlyError(reason)}`);
    }
  };

  const cancelTtsPreview = async () => {
    // Invalidate synchronously; an old cancellation reply must never clear a
    // replacement request that was started while the IPC call was in flight.
    ttsPlayerRef.current?.dispose();
    setTtsError("");
  };

  const updateHotkey = async (action: "record" | "recopy", value: string) => {
    try {
      await invoke("rebind_hotkey", { action, shortcutStr: value });
      if (action === "record") setRecordHotkey(value);
      else setRecopyHotkey(value);
      showNotification("Shortcut updated");
    } catch (reason) {
      setError(`Shortcut could not be updated. ${friendlyError(reason)}`);
    } finally {
      setEditingHotkey(null);
    }
  };

  const openExternalKeyUrl = async (url: string) => {
    let parsed: URL;
    try {
      parsed = new URL(url);
      if (parsed.protocol !== "https:") throw new Error("Unsupported link");
    } catch {
      setError("This provider key link is invalid.");
      return;
    }
    if (!isTauriRuntime()) {
      window.open(parsed.href, "_blank", "noopener,noreferrer");
      return;
    }
    try {
      await open(parsed.href);
    } catch (reason) {
      setError(`Could not open the provider website. ${friendlyError(reason)}`);
    }
  };

  const selectedProvider = PROVIDERS[transcriptionProvider];
  const selectedFormattingProvider = PROVIDERS[formattingProvider];

  if (booting) {
    return (
      <main className="boot-screen" aria-label="Loading OpenFlow">
        <img src="/logo-128.png" alt="" className="boot-logo" />
        <span className="boot-pulse" />
      </main>
    );
  }

  if (screen === "onboarding") {
    const currentStep = STEP_ORDER.indexOf(onboardingStep);
    return (
      <main className="onboarding-page">
        <section className="onboarding-brand" aria-label="OpenFlow introduction">
          <div className="brand-lockup"><img src="/logo-128.png" alt="" /><span>OpenFlow</span></div>
          <div className="brand-copy">
            <span className="eyebrow">Private by design</span>
            <h1>Say it once.<br/><em>Keep moving.</em></h1>
            <p>Hold a shortcut, speak naturally, and polished text lands where you’re working.</p>
          </div>
          <div className="privacy-note"><Icon name="spark" /><span>Your key stays on this device. Audio goes only to the provider you choose.</span></div>
        </section>

        <section className="onboarding-panel" aria-labelledby="setup-title">
          <header className="onboarding-header">
            <div>
              <span className="step-kicker">Setup · {currentStep + 1} of {STEP_ORDER.length}</span>
              <h2 id="setup-title">{onboardingStep === "provider" ? "Choose how OpenFlow listens" : onboardingStep === "credentials" ? "Connect your provider" : "Make it yours"}</h2>
            </div>
            <ol className="progress-dots" aria-label="Setup progress">
              {STEP_ORDER.map((step, index) => (
                <li key={step} className={index < currentStep ? "complete" : index === currentStep ? "active" : ""} aria-current={index === currentStep ? "step" : undefined}>
                  {index < currentStep ? <Icon name="check" size={13} /> : index + 1}
                </li>
              ))}
            </ol>
          </header>

          {onboardingStep === "provider" && (
            <div className="panel-content flow-stack">
              <p className="section-lead">Groq is the fastest path: one key covers Whisper transcription, cleanup, and Orpheus voice.</p>
              <fieldset className="provider-grid">
                <legend className="sr-only">Transcription provider</legend>
                {Object.entries(PROVIDERS).map(([key, provider]) => (
                  <label key={key} className={`provider-option ${transcriptionProvider === key ? "selected" : ""}`}>
                    <input type="radio" name="provider" value={key} checked={transcriptionProvider === key} onChange={() => setProvider("transcription", key)} />
                    <span className="provider-copy"><strong>{provider.label}</strong><small>{provider.description}</small></span>
                    {provider.recommended && <span className="recommended-badge">Recommended</span>}
                    <span className="radio-mark" aria-hidden="true" />
                  </label>
                ))}
              </fieldset>
              <div className="inline-setting">
                <div><strong>Use the same provider for cleanup</strong><small>{transcriptionProvider === "deepgram" ? "Deepgram handles speech only; choose a separate cleanup provider." : "Best for a simple, one-key setup."}</small></div>
                <Toggle checked={sameProvider} disabled={transcriptionProvider === "deepgram"} onChange={(checked) => { setSameProvider(checked); if (checked) { setFormattingProvider(transcriptionProvider); setFormattingKey(transcriptionKey); setFormattingModel(PROVIDERS[transcriptionProvider].chatDefault); } markDirty(); }} label="Use same provider for cleanup" />
              </div>
              {!sameProvider && (
                <div className="field-group reveal">
                  <label htmlFor="formatting-provider">Formatting provider</label>
                  <select id="formatting-provider" value={formattingProvider} onChange={(event) => setProvider("formatting", event.target.value)}>
                    {Object.entries(PROVIDERS).filter(([key]) => key !== "deepgram").map(([key, provider]) => <option key={key} value={key}>{provider.label}</option>)}
                  </select>
                </div>
              )}
              <div className="panel-actions end"><button className="button primary" onClick={() => setOnboardingStep("credentials")}>Continue to connection <span aria-hidden="true">→</span></button></div>
            </div>
          )}

          {onboardingStep === "credentials" && (
            <div className="panel-content flow-stack">
              <p className="section-lead">We’ll verify access before saving anything. OpenFlow never sends your key anywhere else.</p>
              {transcriptionProvider === "custom" && (
                <div className="field-group">
                  <label htmlFor="custom-transcription-url">Transcription endpoint</label>
                  <input id="custom-transcription-url" type="url" value={customTranscriptionUrl} onChange={(event) => { setCustomTranscriptionUrl(event.target.value); markDirty(); }} placeholder="https://your-server.com/v1" autoCapitalize="none" autoCorrect="off" />
                </div>
              )}
              <div className="field-group">
                <div className="label-row"><label htmlFor="transcription-key">{selectedProvider.label} API key</label>{selectedProvider.keyUrl && <a href={selectedProvider.keyUrl} onClick={(event) => { event.preventDefault(); void openExternalKeyUrl(selectedProvider.keyUrl); }}>Create a key ↗</a>}</div>
                <div className="secret-input">
                  <input id="transcription-key" type={showTranscriptionKey ? "text" : "password"} value={transcriptionKey} onChange={(event) => { setTranscriptionKey(event.target.value); if (sameProvider) setFormattingKey(event.target.value); setConnectionState("idle"); setConnectionMessage(""); }} placeholder={selectedProvider.keyPlaceholder} autoCapitalize="none" autoCorrect="off" spellCheck={false} />
                  <button type="button" onClick={() => setShowTranscriptionKey((shown) => !shown)} aria-label={showTranscriptionKey ? "Hide API key" : "Show API key"}>{showTranscriptionKey ? "Hide" : "Show"}</button>
                </div>
                <small className="field-help">Stored in your operating system’s protected credential store.</small>
              </div>

              {!sameProvider && (
                <div className="secondary-credentials reveal">
                  {formattingProvider === "custom" && (
                    <div className="field-group">
                      <label htmlFor="custom-formatting-url">Formatting endpoint</label>
                      <input id="custom-formatting-url" type="url" value={customFormattingUrl} onChange={(event) => { setCustomFormattingUrl(event.target.value); markDirty(); }} placeholder="https://your-server.com/v1" autoCapitalize="none" autoCorrect="off" />
                    </div>
                  )}
                  <div className="field-group">
                    <div className="label-row"><label htmlFor="formatting-key">{selectedFormattingProvider.label} formatting key</label>{selectedFormattingProvider.keyUrl && <a href={selectedFormattingProvider.keyUrl} onClick={(event) => { event.preventDefault(); void openExternalKeyUrl(selectedFormattingProvider.keyUrl); }}>Create a key ↗</a>}</div>
                    <div className="secret-input">
                      <input id="formatting-key" type={showFormattingKey ? "text" : "password"} value={formattingKey} onChange={(event) => { setFormattingKey(event.target.value); setConnectionState("idle"); setConnectionMessage(""); }} placeholder={selectedFormattingProvider.keyPlaceholder} autoCapitalize="none" autoCorrect="off" spellCheck={false} />
                      <button type="button" onClick={() => setShowFormattingKey((shown) => !shown)} aria-label={showFormattingKey ? "Hide formatting key" : "Show formatting key"}>{showFormattingKey ? "Hide" : "Show"}</button>
                    </div>
                  </div>
                </div>
              )}

              {connectionMessage && <div className={`connection-state ${connectionState}`} role={connectionState === "error" ? "alert" : "status"}><Icon name={connectionState === "connected" ? "check" : "spark"} /><span>{connectionMessage}</span></div>}
              <div className="panel-actions split">
                <button className="button quiet" onClick={() => setOnboardingStep("provider")}><Icon name="arrow" /> Back</button>
                {connectionState === "connected" ? (
                  <button className="button primary" onClick={() => setOnboardingStep("models")}>Continue to preferences <span aria-hidden="true">→</span></button>
                ) : (
                  <button className="button primary" disabled={connectionState === "checking" || modelsLoading !== null} onClick={() => void verifyConnection()}>{connectionState === "checking" || modelsLoading !== null ? <><span className="spinner" /> Checking access…</> : "Verify connection"}</button>
                )}
              </div>
            </div>
          )}

          {onboardingStep === "models" && (
            <div className="panel-content flow-stack">
              <p className="section-lead">Smart defaults are ready. Change a model now or paste any compatible model ID later.</p>
              <div className="preference-grid">
                <div className="field-group">
                  <label htmlFor="onboarding-stt-model">Speech-to-text model</label>
                  <input id="onboarding-stt-model" list="stt-models" value={transcriptionModel} onChange={(event) => setTranscriptionModel(event.target.value)} placeholder={selectedProvider.sttDefault} autoCapitalize="none" autoCorrect="off" spellCheck={false} />
                  <datalist id="stt-models">{transcriptionModels.map((model) => <option key={model.id} value={model.id}>{model.name}</option>)}</datalist>
                </div>
                <div className="field-group">
                  <label htmlFor="onboarding-chat-model">Cleanup model</label>
                  <input id="onboarding-chat-model" list="chat-models" value={formattingModel} onChange={(event) => setFormattingModel(event.target.value)} placeholder={selectedFormattingProvider.chatDefault} autoCapitalize="none" autoCorrect="off" spellCheck={false} disabled={!formatEnabled} />
                  <datalist id="chat-models">{formattingModels.map((model) => <option key={model.id} value={model.id}>{model.name}</option>)}</datalist>
                </div>
              </div>
              {(transcriptionProvider === "groq" || transcriptionProvider === "openrouter") && (
                <div className="field-group voice-model-field">
                  <div className="label-row"><label htmlFor="onboarding-tts-model">Voice model</label><span className="capability-badge"><Icon name="volume" size={13} /> Streaming</span></div>
                  <input id="onboarding-tts-model" list="tts-models" value={ttsModel} onChange={(event) => setTtsModel(event.target.value)} placeholder={TTS_DEFAULTS[transcriptionProvider]?.model ?? ""} autoCapitalize="none" autoCorrect="off" spellCheck={false} />
                  <datalist id="tts-models">{ttsModels.map((model) => <option key={model.id} value={model.id}>{model.name}</option>)}</datalist>
                </div>
              )}
              <div className="field-group">
                <div className="label-row"><label htmlFor="onboarding-microphone">Microphone</label><button className="text-button" onClick={() => void loadMicrophones()} disabled={microphonesLoading}><Icon name="refresh" size={14} /> {microphonesLoading ? "Checking…" : "Refresh"}</button></div>
                <select id="onboarding-microphone" value={microphone} onChange={(event) => setMicrophone(event.target.value)}>
                  <option value="">System default</option>
                  {microphones.map((device) => <option key={device.id} value={device.id}>{device.name}{device.is_default ? " · default" : ""}</option>)}
                </select>
                <small className={`field-help mic-readiness ${microphones.length ? "ready" : ""}`}>{microphonesLoading ? "Checking microphone access…" : microphones.length ? `${microphones.length} microphone${microphones.length === 1 ? "" : "s"} ready. You’ll record a first sample in the workspace.` : "No microphone detected yet. Check system permission, then refresh."}</small>
              </div>
              {error && <div className="error-banner" role="alert">{error}</div>}
              <div className="panel-actions split">
                <button className="button quiet" onClick={() => setOnboardingStep("credentials")}><Icon name="arrow" /> Back</button>
                <button className="button primary" disabled={savingSettings} onClick={() => void finishOnboarding()}>{savingSettings ? <><span className="spinner" /> Saving…</> : "Open my workspace"}</button>
              </div>
            </div>
          )}
        </section>
      </main>
    );
  }

  const screenHeader = (title: string, back: () => void, action?: ReactNode) => (
    <header className="screen-header">
      <button className="icon-button back-button" onClick={back} aria-label="Go back"><Icon name="arrow" /></button>
      <div><span className="screen-kicker">OpenFlow</span><h1>{title}</h1></div>
      <div className="header-action">{action}</div>
    </header>
  );

  if (screen === "history") {
    return (
      <main className="app-page">
        {notification && <div className="toast" role="status">{notification}</div>}
        {screenHeader("History", () => setScreen("main"))}
        <div className="page-body history-body">
          <form className="search-field" onSubmit={(event) => { event.preventDefault(); void handleSearch(); }}>
            <input value={searchQuery} onChange={(event) => setSearchQuery(event.target.value)} placeholder="Search what you’ve said" aria-label="Search transcriptions" />
            <button type="submit">Search</button>
          </form>
          {error && <div className="error-banner" role="alert">{error}</div>}
          <div className="history-list">
            {history.length === 0 && <div className="empty-state"><Icon name="clock" size={26} /><h2>Nothing here yet</h2><p>Your first transcription will appear here, ready to copy again.</p><button className="button secondary" onClick={() => setScreen("main")}>Record something</button></div>}
            {history.map((item) => (
              <div key={item.id} className="history-item-row">
                <button className="history-item" onClick={async () => { await copyToClipboard(item.formatted_text || item.raw_text); }}>
                  <span className="history-text">{item.formatted_text || item.raw_text}</span>
                  <span className="history-meta"><time dateTime={item.created_at}>{new Date(item.created_at).toLocaleString(undefined, { dateStyle: "medium", timeStyle: "short" })}</time>{item.duration_ms ? <span>{(item.duration_ms / 1000).toFixed(1)} sec</span> : null}<span>{item.provider.replace("custom:", "")}</span></span>
                </button>
                <button className="history-delete" aria-label="Delete this transcription" title="Delete" onClick={() => void handleDeleteTranscription(item.id)}>×</button>
              </div>
            ))}
          </div>
        </div>
      </main>
    );
  }

  if (screen === "plugins") {
    return (
      <main className="app-page">
        {screenHeader("Plugins", () => setScreen("settings"))}
        <div className="page-body">
          {error && <div className="error-banner" role="alert">{error}</div>}
          <section className="settings-section">
            <div className="section-heading"><div><h2>Installed extensions</h2><p>Small local tools that react to your transcriptions.</p></div></div>
            {plugins.length === 0 ? <div className="empty-state compact"><Icon name="spark" size={24} /><h3>No plugins installed</h3><p>Place a plugin manifest in <code>~/.openflow/plugins/</code>, then reopen this page.</p></div> : (
              <div className="plugin-list">{plugins.map((plugin) => (
                <div key={plugin.manifest.id} className="plugin-row"><div><div className="plugin-title"><strong>{plugin.manifest.name}</strong><code>{plugin.manifest.version}</code></div><p>{plugin.manifest.description}</p></div><Toggle checked={plugin.enabled} label={`${plugin.enabled ? "Disable" : "Enable"} ${plugin.manifest.name}`} onChange={async (enabled) => { try { await invoke(enabled ? "enable_plugin" : "disable_plugin", { id: plugin.manifest.id }); await loadPlugins(); } catch (reason) { setError(friendlyError(reason)); } }} /></div>
              ))}</div>
            )}
          </section>
        </div>
      </main>
    );
  }

  if (screen === "settings") {
    const voiceReady = ttsProvider === "custom" || transcriptionProvider === ttsProvider;
    const voiceLabel = PROVIDERS[ttsProvider]?.label ?? ttsProvider;
    return (
      <main className="app-page">
        {notification && <div className="toast" role="status">{notification}</div>}
        {screenHeader("Settings", () => setScreen("main"), <button className="button primary compact-button" onClick={() => void saveSettings()} disabled={savingSettings || !settingsDirty}>{savingSettings ? "Saving…" : settingsDirty ? "Save" : "Saved"}</button>)}
        <div className="page-body settings-body">
          {error && <div className="error-banner" role="alert">{error}</div>}
          <section className="settings-section">
            <div className="section-heading"><div><h2>Providers</h2><p>Choose where audio and text are processed.</p></div><span className="section-number">01</span></div>
            <div className="settings-grid">
              <div className="field-group"><label htmlFor="settings-provider">Transcription provider</label><select id="settings-provider" value={transcriptionProvider} onChange={(event) => setProvider("transcription", event.target.value)}>{Object.entries(PROVIDERS).map(([key, provider]) => <option key={key} value={key}>{provider.label}</option>)}</select></div>
              {transcriptionProvider === "custom" && <div className="field-group"><label htmlFor="settings-custom-url">Endpoint URL</label><input id="settings-custom-url" type="url" value={customTranscriptionUrl} onChange={(event) => { setCustomTranscriptionUrl(event.target.value); markDirty(); }} /></div>}
              <div className="field-group span-two"><div className="label-row"><label htmlFor="settings-key">API key</label>{selectedProvider.keyUrl && <a href={selectedProvider.keyUrl} onClick={(event) => { event.preventDefault(); void openExternalKeyUrl(selectedProvider.keyUrl); }}>Manage key ↗</a>}</div><div className="secret-input"><input id="settings-key" type={showTranscriptionKey ? "text" : "password"} value={transcriptionKey} onChange={(event) => { setTranscriptionKey(event.target.value); if (sameProvider) setFormattingKey(event.target.value); markDirty(); }} autoCapitalize="none" autoCorrect="off" spellCheck={false} /><button onClick={() => setShowTranscriptionKey((shown) => !shown)}>{showTranscriptionKey ? "Hide" : "Show"}</button></div></div>
              <div className="inline-setting span-two"><div><strong>Use for text cleanup too</strong><small>{transcriptionProvider === "deepgram" ? "Deepgram handles speech only; use another provider here." : "Keep one provider and one key."}</small></div><Toggle checked={sameProvider} disabled={transcriptionProvider === "deepgram"} label="Use same provider for text cleanup" onChange={(checked) => { setSameProvider(checked); if (checked) { setFormattingProvider(transcriptionProvider); setFormattingKey(transcriptionKey); } markDirty(); }} /></div>
              {!sameProvider && <><div className="field-group"><label htmlFor="settings-format-provider">Formatting provider</label><select id="settings-format-provider" value={formattingProvider} onChange={(event) => setProvider("formatting", event.target.value)}>{Object.entries(PROVIDERS).filter(([key]) => key !== "deepgram").map(([key, provider]) => <option key={key} value={key}>{provider.label}</option>)}</select></div><div className="field-group"><label htmlFor="settings-format-key">Formatting key</label><div className="secret-input"><input id="settings-format-key" type={showFormattingKey ? "text" : "password"} value={formattingKey} onChange={(event) => { setFormattingKey(event.target.value); markDirty(); }} autoCapitalize="none" autoCorrect="off" spellCheck={false} /><button onClick={() => setShowFormattingKey((shown) => !shown)}>{showFormattingKey ? "Hide" : "Show"}</button></div></div></>}
            </div>
            <div className="section-footer"><div>{connectionMessage && <span className={`inline-status ${connectionState}`}>{connectionMessage}</span>}</div><button className="button secondary" onClick={() => void verifyConnection()} disabled={connectionState === "checking" || modelsLoading !== null}><Icon name="refresh" size={15} /> {connectionState === "checking" || modelsLoading !== null ? "Checking…" : "Test connection"}</button></div>
          </section>

          <section className="settings-section">
            <div className="section-heading"><div><h2>Models & output</h2><p>Paste any compatible model ID, including new releases.</p></div><span className="section-number">02</span></div>
            <div className="settings-grid">
              <div className="field-group"><label htmlFor="settings-stt-model">Speech-to-text</label><input id="settings-stt-model" list="settings-stt-models" value={transcriptionModel} onChange={(event) => { setTranscriptionModel(event.target.value); markDirty(); }} autoCapitalize="none" autoCorrect="off" spellCheck={false} /><datalist id="settings-stt-models">{transcriptionModels.map((model) => <option key={model.id} value={model.id}>{model.name}</option>)}</datalist></div>
              <div className="field-group"><label htmlFor="settings-chat-model">Text cleanup</label><input id="settings-chat-model" list="settings-chat-models" value={formattingModel} disabled={!formatEnabled} onChange={(event) => { setFormattingModel(event.target.value); markDirty(); }} autoCapitalize="none" autoCorrect="off" spellCheck={false} /><datalist id="settings-chat-models">{formattingModels.map((model) => <option key={model.id} value={model.id}>{model.name}</option>)}</datalist></div>
              <div className="field-group"><label htmlFor="settings-language">Language</label><select id="settings-language" value={language} onChange={(event) => { setLanguage(event.target.value); markDirty(); }}>{LANGUAGE_OPTIONS.map(([value, label]) => <option key={value} value={value}>{label}</option>)}</select></div>
              <div className="inline-setting"><div><strong>Smart text cleanup</strong><small>Add punctuation and structure.</small></div><Toggle checked={formatEnabled} label="Enable smart text cleanup" onChange={(checked) => { setFormatEnabled(checked); markDirty(); }} /></div>
              <div className="field-group span-two"><label htmlFor="settings-dictionary">Dictionary</label><textarea id="settings-dictionary" value={dictionary} maxLength={800} rows={2} placeholder="ENTRO.LY, FastPay, TikTok Shop, Lark" onChange={(event) => { setDictionary(event.target.value); markDirty(); }} autoCapitalize="none" autoCorrect="off" spellCheck={false} /><small className="field-help">Names and terms to spell correctly, comma separated. Sent to Whisper as a spelling hint on Groq, OpenAI, and custom endpoints.</small></div>
            </div>
          </section>

          <section className="settings-section voice-section">
            <div className="section-heading"><div><div className="heading-with-badge"><h2>Voice output</h2><span className="capability-badge"><Icon name="volume" size={13} /> Streaming</span></div><p>Preview the voice endpoint before you rely on it.</p></div><span className="section-number">03</span></div>
            <>
                <div className="settings-grid">
                  <div className="inline-setting span-two"><div><strong>Voice features</strong><small>Enable speech generation in OpenFlow.</small></div><Toggle checked={ttsEnabled} label="Enable voice features" onChange={(checked) => { setTtsEnabled(checked); markDirty(); }} /></div>
                  <div className="field-group"><label htmlFor="settings-tts-provider">Speech endpoint</label><select id="settings-tts-provider" value={ttsProvider} disabled={!ttsEnabled} onChange={(event) => { const next = event.target.value; setTtsProvider(next); markDirty(); setTtsStatus("idle"); setTtsModel(""); setTtsVoice(""); }}><option value="groq">Groq (Orpheus)</option><option value="openrouter">OpenRouter (Gemini)</option><option value="openai">OpenAI</option><option value="custom">Self-hosted / LAN</option></select><small>Point this at a machine on your network to keep audio off the internet.</small></div>
                  {ttsProvider === "custom" && (
                    <div className="field-group span-two"><label htmlFor="settings-tts-url">Speech endpoint URL</label><input id="settings-tts-url" value={customTtsUrl} disabled={!ttsEnabled} onChange={(event) => { setCustomTtsUrl(event.target.value); markDirty(); setTtsStatus("idle"); }} placeholder="http://192.168.1.10:8880/v1" autoCapitalize="none" autoCorrect="off" spellCheck={false} /><small>OpenAI-compatible base URL. OpenFlow calls <code>/audio/speech</code> under it. Plain http and an empty API key are both fine on a LAN.</small></div>
                  )}
                  <div className="field-group"><label htmlFor="settings-tts-model">Voice model</label><input id="settings-tts-model" list="settings-tts-models" value={ttsModel} disabled={!ttsEnabled} placeholder={TTS_DEFAULTS[ttsProvider]?.model ?? ""} onChange={(event) => { setTtsModel(event.target.value); markDirty(); setTtsStatus("idle"); }} autoCapitalize="none" autoCorrect="off" spellCheck={false} /><datalist id="settings-tts-models">{ttsModels.map((model) => <option key={model.id} value={model.id}>{model.name}</option>)}</datalist></div>
                  <div className="field-group"><label htmlFor="settings-tts-voice">Voice</label><input id="settings-tts-voice" list="tts-voices" value={ttsVoice} disabled={!ttsEnabled} placeholder={TTS_DEFAULTS[ttsProvider]?.voice ?? ""} onChange={(event) => { setTtsVoice(event.target.value); markDirty(); setTtsStatus("idle"); }} /><datalist id="tts-voices">{(TTS_DEFAULTS[ttsProvider]?.voices ?? []).map((voice) => <option key={voice} value={voice} />)}</datalist></div>
                </div>
                {voiceReady ? <div className="voice-preview">
                  <label htmlFor="tts-preview">Preview text</label>
                  <textarea id="tts-preview" value={ttsPreviewText} maxLength={500} disabled={!ttsEnabled} onChange={(event) => { setTtsPreviewText(event.target.value); setTtsStatus("idle"); setTtsError(""); }} />
                  <div className="voice-preview-footer"><span>{ttsPreviewText.length}/500</span><div className="voice-actions">{ttsStatus === "streaming" ? <button className="button secondary" onClick={() => void cancelTtsPreview()}><Icon name="stop" size={15} /> Stop</button> : <button className="button secondary" disabled={!ttsEnabled || !ttsPreviewText.trim()} onClick={() => void playTtsPreview()}><Icon name="play" size={15} /> {ttsStatus === "ready" ? "Play again" : "Stream preview"}</button>}</div></div>
                  {ttsStatus === "streaming" && <div className="streaming-status" role="status"><span className="stream-bars"><i/><i/><i/><i/></span> Preparing audio…</div>}
                  {ttsError && <div className="error-banner" role="alert">{ttsError}</div>}
                  {ttsAudioUrl && <audio ref={audioRef} className="audio-player" controls src={ttsAudioUrl}>Your browser does not support audio playback.</audio>}
                </div> : (
                  <div className="capability-empty"><Icon name="volume" size={22} /><div><strong>{voiceLabel} key required</strong><p>Hosted voice endpoints reuse your transcription key. Choose {voiceLabel} as the transcription provider too, or switch the speech endpoint to Self-hosted / LAN.</p></div></div>
                )}
            </>
          </section>

          <section className="settings-section">
            <div className="section-heading"><div><h2>Device & shortcuts</h2><p>Control how recording feels on this computer.</p></div><span className="section-number">04</span></div>
            <div className="settings-list">
              <div className="settings-row"><div><label htmlFor="settings-microphone">Microphone</label><small>{microphones.length ? `${microphones.length} available` : "Permission may be required"}</small></div><select id="settings-microphone" value={microphone} onChange={(event) => { setMicrophone(event.target.value); markDirty(); }}><option value="">System default</option>{microphones.map((device) => <option key={device.id} value={device.id}>{device.name}{device.is_default ? " · default" : ""}</option>)}</select></div>
              <div className="settings-row"><div><label htmlFor="record-hotkey">Record shortcut</label><small>Hold to record, release to transcribe.</small></div>{editingHotkey === "record" ? <input id="record-hotkey" className="hotkey-input" aria-label="Record shortcut" autoFocus value={hotkeyDraft} onChange={(event) => setHotkeyDraft(event.target.value)} onBlur={() => { if (cancelHotkeyEditRef.current) { cancelHotkeyEditRef.current = false; return; } void updateHotkey("record", hotkeyDraft); }} onKeyDown={(event) => { if (event.key === "Enter") event.currentTarget.blur(); if (event.key === "Escape") { event.preventDefault(); cancelHotkeyEditRef.current = true; setEditingHotkey(null); } }} /> : <button id="record-hotkey" className="key-button" aria-label={`Change record shortcut, currently ${recordHotkey}`} onClick={() => { cancelHotkeyEditRef.current = false; setHotkeyDraft(recordHotkey); setEditingHotkey("record"); }}>{recordHotkey}</button>}</div>
              <div className="settings-row"><div><label htmlFor="recopy-hotkey">Re-copy shortcut</label><small>Paste your most recent result again.</small></div>{editingHotkey === "recopy" ? <input id="recopy-hotkey" className="hotkey-input" aria-label="Re-copy shortcut" autoFocus value={hotkeyDraft} onChange={(event) => setHotkeyDraft(event.target.value)} onBlur={() => { if (cancelHotkeyEditRef.current) { cancelHotkeyEditRef.current = false; return; } void updateHotkey("recopy", hotkeyDraft); }} onKeyDown={(event) => { if (event.key === "Enter") event.currentTarget.blur(); if (event.key === "Escape") { event.preventDefault(); cancelHotkeyEditRef.current = true; setEditingHotkey(null); } }} /> : <button id="recopy-hotkey" className="key-button" aria-label={`Change re-copy shortcut, currently ${recopyHotkey}`} onClick={() => { cancelHotkeyEditRef.current = false; setHotkeyDraft(recopyHotkey); setEditingHotkey("recopy"); }}>{recopyHotkey}</button>}</div>
              <div className="settings-row"><div><label>Insert transcriptions by</label><small>Pasting sends Cmd+V and works in every app. Typing sends the characters themselves, so nothing waits on a paste, but the receiving app may autocorrect them. Typing is macOS only; elsewhere this pastes.</small></div><div className="segmented"><button className={insertMethod === "paste" ? "active" : ""} onClick={() => { setInsertMethod("paste"); markDirty(); }}>Paste</button><button className={insertMethod === "type" ? "active" : ""} onClick={() => { setInsertMethod("type"); markDirty(); }}>Type</button></div></div>
              <div className="settings-row"><div><label>Keep my clipboard</label><small>Puts back whatever you had copied once a dictation has been inserted, so dictating never costs you a copy. With Type, the clipboard is not touched at all. Text and images are restored; copied files are not. The re-copy shortcut and tray recents still copy on purpose.</small></div><Toggle checked={preserveClipboard} label="Keep clipboard contents across dictations" onChange={(checked) => { setPreserveClipboard(checked); markDirty(); }} /></div>
              <div className="settings-row"><div><label>Hide the overlay until recording</label><small>The overlay normally rests against the edge of the screen. Hidden, it appears when you start recording and goes away once the text lands. You can only reposition it while it is showing.</small></div><button className="button secondary compact-button" onClick={() => { setOverlayOnlyWhileRecording((on) => !on); markDirty(); }} aria-pressed={overlayOnlyWhileRecording}>{overlayOnlyWhileRecording ? "On" : "Off"}</button></div>
              <div className="settings-row"><div><label>Appearance</label><small>Use the theme that feels easiest on your eyes.</small></div><div className="segmented"><button className={theme === "light" ? "active" : ""} onClick={() => { setTheme("light"); markDirty(); }}>Light</button><button className={theme === "dark" ? "active" : ""} onClick={() => { setTheme("dark"); markDirty(); }}>Dark</button></div></div>
              <div className="settings-row"><div><label>Save transcription history</label><small>Dictation captures whatever you say out loud. Turn this off to keep nothing.</small></div><button className="button secondary compact-button" onClick={() => { setSaveHistory((on) => !on); setSettingsDirty(true); }} aria-pressed={saveHistory}>{saveHistory ? "On" : "Off"}</button></div>
              <div className="settings-row"><div><label>Auto-delete after</label><small>Older entries are removed on launch and after each transcription.</small></div><select value={retentionDays} onChange={(event) => { setRetentionDays(event.target.value); setSettingsDirty(true); }}><option value="">Never</option><option value="1">1 day</option><option value="7">7 days</option><option value="30">30 days</option><option value="90">90 days</option></select></div>
              <div className="settings-row"><div><label>Clear all history</label><small>Deletes every stored transcription immediately.</small></div><button className="button secondary compact-button danger" onClick={() => void handleClearHistory()} onBlur={() => setConfirmingClear(false)}>{confirmingClear ? "Click again to confirm" : "Clear"}</button></div>
              <div className="settings-row"><div><label>Plugins</label><small>Extend what happens after transcription.</small></div><button className="button secondary compact-button" onClick={() => { void loadPlugins(); setScreen("plugins"); }}>Manage</button></div>
            </div>
          </section>
        </div>
      </main>
    );
  }

  return (
    <main className="workspace">
      {notification && <div className="toast" role="status">{notification}</div>}
      <header className="workspace-header">
        <button className="icon-button" onClick={() => { void loadHistory(); setScreen("history"); }} aria-label="Open transcription history"><Icon name="clock" /></button>
        <button className="brand-button" onClick={() => { if (!history.length) void loadHistory(); setShowRecents((shown) => !shown); }} aria-expanded={showRecents} aria-haspopup="dialog"><img src="/logo-128.png" alt=""/><span>OpenFlow</span></button>
        <button className="icon-button" onClick={() => { setError(""); setSettingsDirty(false); setScreen("settings"); }} aria-label="Open settings"><Icon name="gear" /></button>
        {showRecents && <div className="recents-popover" role="dialog" aria-label="Recent transcriptions"><div className="popover-heading"><span>Recent</span><button onClick={() => setShowRecents(false)} aria-label="Close recent transcriptions">×</button></div>{history.length === 0 ? <p className="popover-empty">Your recent words will appear here.</p> : history.slice(0, 6).map((item) => <button key={item.id} onClick={async () => { await copyToClipboard(item.formatted_text || item.raw_text); setShowRecents(false); }}>{item.formatted_text || item.raw_text}</button>)}{history.length > 0 && <button className="popover-footer" onClick={() => { setShowRecents(false); setScreen("history"); }}>View all history</button>}</div>}
      </header>

      <section className="recording-workspace" aria-labelledby="recording-title">
        <div className={`voice-orbit ${status}`} aria-hidden="true"><span className="orbit-line one"/><span className="orbit-line two"/><div className="voice-core"><Icon name="mic" size={30} /></div></div>
        <div className="recording-copy"><span className="eyebrow">{status === "idle" ? "Ready when you are" : status === "recording" ? "Listening now" : "Turning speech into text"}</span><h1 id="recording-title">{status === "idle" ? "Hold to speak" : status === "recording" ? "Keep talking…" : "One moment…"}</h1><p>{status === "idle" ? "Release when you’re done. OpenFlow cleans it up and pastes it for you." : status === "recording" ? "Your audio is captured only while you hold the button." : "Your provider is transcribing and formatting the result."}</p></div>
        <button
          className={`record-button ${status}`}
          disabled={status === "transcribing"}
          onPointerDown={(event) => { event.currentTarget.setPointerCapture(event.pointerId); void handleStartRecording(); }}
          onPointerUp={() => void handleStopRecording()}
          onPointerCancel={() => void handleStopRecording()}
          onKeyDown={(event) => { if ((event.key === " " || event.key === "Enter") && !event.repeat) { event.preventDefault(); void handleStartRecording(); } }}
          onKeyUp={(event) => { if (event.key === " " || event.key === "Enter") { event.preventDefault(); void handleStopRecording(); } }}
        >
          {status === "idle" && <><Icon name="mic" /> Hold to record</>}
          {status === "recording" && <><span className="live-dot" /> Release to finish</>}
          {status === "transcribing" && <><span className="spinner" /> Transcribing…</>}
        </button>
        {status === "transcribing" && <button className="button quiet cancel-transcription" onClick={() => void cancelTranscription()}><Icon name="stop" size={15} /> Cancel transcription</button>}
        <p className="shortcut-hint"><kbd>{recordHotkey}</kbd> works from any app <span>·</span> <kbd>{recopyHotkey}</kbd> pastes again</p>
      </section>

      <section className="workspace-feedback" aria-live="polite">
        {error && <div className="error-banner" role="alert"><span>{error}</span><button onClick={() => setError("")} aria-label="Dismiss error">×</button></div>}
        {lastTranscription ? <button className="last-result" onClick={async () => { await copyToClipboard(lastTranscription); }}><span className="result-status"><Icon name="check" size={15} /> Copied to clipboard</span><span className="result-copy">{lastTranscription}</span><span className="copy-affordance">Copy again</span></button> : <div className="first-sample"><span className="sample-line"/><p>Your first transcription will settle here.</p><span className="sample-line"/></div>}
      </section>
    </main>
  );
}

export default App;
