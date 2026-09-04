//! The Tauri host: windows, tray, global shortcuts, and the command surface
//! the webview calls. Everything it does to audio, text or storage goes
//! through [`openflow_core::engine::Engine`].

use openflow_core::audio::AudioDevice;
use openflow_core::db::Transcription;
use openflow_core::engine::{Engine, EngineEvent, EngineEvents};
use openflow_core::hotkey;
use openflow_core::plugins::PluginInfo;
use openflow_core::speech::{SpeechAudio, SpeechRequest, SpeechResult};
use openflow_core::transcribe::ModelInfo;
use std::sync::Arc;
use tauri::menu::{MenuBuilder, MenuItemBuilder};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};

/// The engine's events, forwarded to the webview under the names `App.tsx` and
/// `overlay.html` listen for.
struct TauriEvents {
    app: AppHandle,
}

impl EngineEvents for TauriEvents {
    fn emit(&self, event: EngineEvent) -> Result<(), String> {
        let sent = match event {
            EngineEvent::RecordingState(state) => self.app.emit("recording-state", state.as_str()),
            EngineEvent::TranscriptionResult(transcription) => {
                self.app.emit("transcription-result", &transcription)
            }
            EngineEvent::TranscriptionPartial(partial) => {
                self.app.emit("transcription-partial", &partial)
            }
            EngineEvent::TranscriptionWarning(warning) => {
                self.app.emit("transcription-warning", warning)
            }
            EngineEvent::TranscriptionError(error) => self.app.emit("transcription-error", error),
            EngineEvent::RecopySuccess(message) => self.app.emit("recopy-success", message),
            // Not a webview event: the tray's recents list is what went stale.
            EngineEvent::HistoryChanged => {
                update_tray_menu(&self.app);
                return Ok(());
            }
            EngineEvent::TtsStarted(payload) => self.app.emit("tts-started", &payload),
            EngineEvent::TtsChunk(payload) => self.app.emit("tts-audio-chunk", payload),
            EngineEvent::TtsFinished(payload) => self.app.emit("tts-finished", &payload),
            EngineEvent::TtsError(payload) => self.app.emit("tts-error", payload),
            // The webview has no local-runner UI (Milestone C retires this
            // host), but the event is forwarded rather than dropped so a
            // console or a future screen can see it without a new plumbing pass.
            EngineEvent::RunnerState(payload) => self.app.emit("runner-state", &payload),
            EngineEvent::PreviewAgreement(payload) => self.app.emit("preview-agreement", &payload),
            EngineEvent::Navigate(target) => self.app.emit("navigate", target),
        };
        sent.map_err(|error| error.to_string())
    }
}

// ── Settings ──────────────────────────────────────────────
#[tauri::command]
fn set_api_key(engine: State<Arc<Engine>>, key: String) -> Result<(), String> {
    engine.settings().set("api_key", &key)
}

#[tauri::command]
fn get_api_key(engine: State<Arc<Engine>>) -> Result<Option<String>, String> {
    engine.settings().get("api_key")
}

#[tauri::command]
fn get_setting(engine: State<Arc<Engine>>, key: String) -> Result<Option<String>, String> {
    engine.settings().get(&key)
}

#[tauri::command]
fn set_setting(engine: State<Arc<Engine>>, key: String, value: String) -> Result<(), String> {
    engine.settings().set(&key, &value)
}

// ── History ───────────────────────────────────────────────
#[tauri::command]
fn get_history(
    engine: State<Arc<Engine>>,
    limit: Option<usize>,
) -> Result<Vec<Transcription>, String> {
    engine.history(limit.unwrap_or(50))
}

#[tauri::command]
fn search_history(engine: State<Arc<Engine>>, query: String) -> Result<Vec<Transcription>, String> {
    engine.search_history(&query, 50)
}

#[tauri::command]
fn delete_transcription(engine: State<Arc<Engine>>, id: String) -> Result<(), String> {
    engine.delete_transcription(&id)
}

#[tauri::command]
fn clear_history(engine: State<Arc<Engine>>) -> Result<usize, String> {
    engine.clear_history()
}

// ── Models + Devices ──────────────────────────────────────
#[tauri::command]
async fn fetch_models(
    engine: State<'_, Arc<Engine>>,
    provider_name: Option<String>,
    api_key_override: Option<String>,
) -> Result<Vec<ModelInfo>, String> {
    engine.fetch_models(provider_name, api_key_override).await
}

#[tauri::command]
fn list_audio_devices(engine: State<Arc<Engine>>) -> Result<Vec<AudioDevice>, String> {
    engine.list_audio_devices()
}

// ── Speech ────────────────────────────────────────────────
#[tauri::command]
async fn synthesize_speech(
    engine: State<'_, Arc<Engine>>,
    text: String,
    model: Option<String>,
    voice: Option<String>,
    response_format: Option<String>,
) -> Result<SpeechAudio, String> {
    engine
        .synthesize_speech(&SpeechRequest {
            text,
            model,
            voice,
            response_format,
            request_id: None,
        })
        .await
}

#[tauri::command]
async fn stream_speech(
    engine: State<'_, Arc<Engine>>,
    text: String,
    model: Option<String>,
    voice: Option<String>,
    response_format: Option<String>,
    request_id: Option<String>,
) -> Result<SpeechResult, String> {
    engine
        .stream_speech(SpeechRequest {
            text,
            model,
            voice,
            response_format,
            request_id,
        })
        .await
}

#[tauri::command]
fn cancel_speech(engine: State<Arc<Engine>>, request_id: Option<String>) -> Result<bool, String> {
    engine.cancel_speech(request_id.as_deref())
}

// ── Recording ─────────────────────────────────────────────
#[tauri::command]
fn start_recording(engine: State<Arc<Engine>>) -> Result<(), String> {
    engine.start_recording()
}

#[tauri::command]
async fn stop_recording_and_transcribe(
    engine: State<'_, Arc<Engine>>,
) -> Result<Transcription, String> {
    engine.stop_and_transcribe_now().await
}

#[tauri::command]
fn cancel_current_transcription(engine: State<Arc<Engine>>) -> Result<bool, String> {
    engine.cancel_transcription()
}

// ── Insertion ─────────────────────────────────────────────
#[tauri::command]
fn copy_last_transcription(engine: State<Arc<Engine>>) -> Result<(), String> {
    engine.copy_last_transcription()
}

#[tauri::command]
fn copy_text(engine: State<Arc<Engine>>, text: String) -> Result<(), String> {
    engine.copy_text(&text)
}

/// Copy and paste in one step, for callers that want the keystroke.
#[tauri::command]
fn paste_text(engine: State<Arc<Engine>>, text: String) -> Result<(), String> {
    engine.paste_text(&text)
}

// ── Hotkey management ─────────────────────────────────────
#[tauri::command]
fn rebind_hotkey(
    app: AppHandle,
    engine: State<Arc<Engine>>,
    action: String,
    shortcut_str: String,
) -> Result<(), String> {
    if hotkey::default_shortcut(&action).is_none() {
        return Err("Unknown hotkey action".to_string());
    }
    let gs = app.global_shortcut();
    let new_shortcut = hotkey::parse_shortcut(&shortcut_str)
        .map_err(|e| format!("Invalid shortcut '{}': {}", shortcut_str, e))?;
    let old = engine.settings().shortcut(&action).ok();
    let old_was_registered = old
        .as_ref()
        .map(|shortcut| gs.is_registered(*shortcut))
        .unwrap_or(false);
    if old.as_ref() == Some(&new_shortcut) && old_was_registered {
        return engine.settings().set_hotkey(&action, &shortcut_str);
    }
    gs.register(new_shortcut)
        .map_err(|e| format!("Failed to register shortcut: {}", e))?;
    if let Some(old_shortcut) = old.filter(|_| old_was_registered) {
        if let Err(error) = gs.unregister(old_shortcut) {
            let _ = gs.unregister(new_shortcut);
            return Err(format!("Could not replace the old shortcut: {}", error));
        }
    }
    if let Err(error) = engine.settings().set_hotkey(&action, &shortcut_str) {
        let _ = gs.unregister(new_shortcut);
        if let Some(old_shortcut) = old.filter(|_| old_was_registered) {
            let _ = gs.register(old_shortcut);
        }
        return Err(error);
    }
    Ok(())
}

// ── Plugins ───────────────────────────────────────────────
#[tauri::command]
fn list_plugins(engine: State<Arc<Engine>>) -> Vec<PluginInfo> {
    engine.plugins().list_plugins()
}

#[tauri::command]
fn enable_plugin(engine: State<Arc<Engine>>, id: String) -> Result<(), String> {
    engine.plugins().enable_plugin(&id)
}

#[tauri::command]
fn disable_plugin(engine: State<Arc<Engine>>, id: String) -> Result<(), String> {
    engine.plugins().disable_plugin(&id)
}

#[tauri::command]
fn install_plugin(engine: State<Arc<Engine>>, manifest: String) -> Result<PluginInfo, String> {
    engine.plugins().install_plugin(&manifest)
}

// ── Windows ───────────────────────────────────────────────
fn show_main_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

// ── Tray menu with recents ────────────────────────────────
fn build_tray_menu(
    app: &AppHandle,
) -> Result<tauri::menu::Menu<tauri::Wry>, Box<dyn std::error::Error>> {
    let engine = app.state::<Arc<Engine>>();
    let recents = engine.history(20).unwrap_or_default();

    let mut builder = MenuBuilder::new(app);

    let show = MenuItemBuilder::with_id("show", "Show OpenFlow").build(app)?;
    builder = builder.item(&show);

    if !recents.is_empty() {
        builder = builder.separator();
        let label = MenuItemBuilder::with_id("_label_recents", "Recent Transcriptions")
            .enabled(false)
            .build(app)?;
        builder = builder.item(&label);

        for t in recents.iter() {
            let text = t.formatted_text.as_deref().unwrap_or(&t.raw_text);
            let preview: String = text.chars().take(40).collect();
            let display = if text.chars().count() > 40 {
                format!("{}...", preview)
            } else {
                preview
            };
            // Key by row id, not list index: indexing raced any transcription
            // that landed between building the menu and clicking it.
            let item = MenuItemBuilder::with_id(format!("recent:{}", t.id), &display).build(app)?;
            builder = builder.item(&item);
        }

        builder = builder.separator();
        let all = MenuItemBuilder::with_id("show_history", "All History...").build(app)?;
        builder = builder.item(&all);
    }

    builder = builder.separator();
    let quit = MenuItemBuilder::with_id("quit", "Quit").build(app)?;
    builder = builder.item(&quit);

    Ok(builder.build()?)
}

fn update_tray_menu(app: &AppHandle) {
    if let Ok(menu) = build_tray_menu(app) {
        if let Some(tray) = app.tray_by_id("main_tray") {
            let _ = tray.set_menu(Some(menu));
        }
    }
}

// ── App entry ─────────────────────────────────────────────
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, event| {
                    let engine = app.state::<Arc<Engine>>().inner().clone();
                    let Ok(record_shortcut) = engine.settings().shortcut("record") else {
                        return;
                    };
                    let Ok(recopy_shortcut) = engine.settings().shortcut("recopy") else {
                        return;
                    };

                    if shortcut == &record_shortcut {
                        match event.state() {
                            ShortcutState::Pressed => engine.hotkey_pressed(),
                            ShortcutState::Released => engine.hotkey_released(),
                        }
                    } else if shortcut == &recopy_shortcut
                        && matches!(event.state(), ShortcutState::Pressed)
                    {
                        engine.recopy();
                    }
                })
                .build(),
        )
        .setup(|app| {
            let app_dir = app
                .path()
                .app_data_dir()
                .map_err(|e| format!("No app dir: {}", e))?;
            let events: Arc<dyn EngineEvents> = Arc::new(TauriEvents {
                app: app.handle().clone(),
            });
            // One runtime for the process: the engine spawns onto Tauri's.
            let engine = Engine::new(
                app_dir,
                events,
                Box::new(|future| {
                    tauri::async_runtime::spawn(future);
                }),
            )?;
            app.manage(engine.clone());

            // Register hotkeys from settings (or defaults)
            let record_shortcut = engine.settings().shortcut("record")?;
            let recopy_shortcut = engine.settings().shortcut("recopy")?;

            app.global_shortcut()
                .register(record_shortcut)
                .unwrap_or_else(|e| eprintln!("Record hotkey failed: {}", e));
            app.global_shortcut()
                .register(recopy_shortcut)
                .unwrap_or_else(|e| eprintln!("Recopy hotkey failed: {}", e));

            // Tray with recents
            let menu = build_tray_menu(app.handle())?;

            let _tray = TrayIconBuilder::with_id("main_tray")
                .menu(&menu)
                .show_menu_on_left_click(true)
                .tooltip("OpenFlow - Ready")
                .icon({
                    let bytes = include_bytes!("../icons/icon.png");
                    tauri::image::Image::from_bytes(bytes)?
                })
                .icon_as_template(true)
                .on_menu_event(|app, event| {
                    let id = event.id().as_ref();
                    match id {
                        "show" => {
                            show_main_window(app);
                        }
                        "show_history" => {
                            show_main_window(app);
                            if let Some(w) = app.get_webview_window("main") {
                                let _ = w.show();
                                let _ = w.set_focus();
                                let _ = app.emit("navigate", "history");
                            }
                        }
                        "quit" => {
                            app.exit(0);
                        }
                        s if s.starts_with("recent:") => {
                            let row_id = &s["recent:".len()..];
                            app.state::<Arc<Engine>>().paste_transcription(row_id);
                        }
                        _ => {}
                    }
                })
                .build(app)?;

            // Make overlay background transparent
            if let Some(overlay) = app.get_webview_window("overlay") {
                let _ = overlay.set_background_color(Some(tauri::utils::config::Color(0, 0, 0, 0)));
            }

            #[cfg(debug_assertions)]
            {
                if let Some(window) = app.get_webview_window("main") {
                    window.open_devtools();
                }
            }

            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "main" {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            set_api_key,
            get_api_key,
            get_setting,
            set_setting,
            get_history,
            search_history,
            delete_transcription,
            clear_history,
            fetch_models,
            list_audio_devices,
            start_recording,
            stop_recording_and_transcribe,
            cancel_current_transcription,
            synthesize_speech,
            stream_speech,
            cancel_speech,
            copy_last_transcription,
            copy_text,
            paste_text,
            rebind_hotkey,
            list_plugins,
            enable_plugin,
            disable_plugin,
            install_plugin,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    app.run(|app_handle, event| {
        #[cfg(target_os = "macos")]
        if matches!(event, tauri::RunEvent::Reopen { .. }) {
            show_main_window(app_handle);
        }
        #[cfg(not(target_os = "macos"))]
        let _ = (app_handle, event);
    });
}
