//! The engine's event sink, and the only place the app crosses a thread
//! boundary inbound.
//!
//! Two facts about `EngineEvents::emit` shape everything here:
//!
//! - It runs on whatever thread finished the work: a tokio worker for the
//!   pipeline and the speech stream, the `global-hotkey` callback thread for a
//!   press or release. None of those may touch AppKit.
//! - It can run while the engine holds its own locks (`emit_idle_if_quiescent`
//!   emits with the recording lock held, on purpose, so a new capture cannot
//!   publish "recording" before a stale "idle"). So the sink must not call back
//!   into the engine synchronously; doing so would deadlock the moment the
//!   engine takes the same lock again.
//!
//! Status events satisfy both by handing work to the main queue and returning.
//! Audio bytes bypass AppKit through a bounded async queue; the decoder thread
//! owns consumption and pending delivery yields to the runtime.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use dispatch2::DispatchQueue;
use openflow_core::engine::{EngineEvent, EngineEvents};
use openflow_core::speech::SpeechChunk;

/// Core delivers at most 64 KiB per chunk. Four chunks keep decoder ingress
/// under 256 KiB; waiting for room suspends the HTTP future, never AppKit.
pub const SPEECH_QUEUE_CHUNKS: usize = 4;

struct Ingress {
    request_id: String,
    sender: Option<tokio::sync::mpsc::Sender<Vec<u8>>>,
    receiver: Option<tokio::sync::mpsc::Receiver<Vec<u8>>>,
    cancelled: Arc<AtomicBool>,
    stopped: tokio::sync::watch::Sender<bool>,
}

/// Whether a voice preview is still listening for audio.
///
/// `speech::stream` stops when a chunk cannot be delivered, which is what keeps
/// a cancelled preview from downloading a clip nobody will hear. The main-queue
/// hop is asynchronous, so "delivered" cannot mean "the player has it"; it means
/// a player for this request id is still open. That is a synchronous read of a
/// plain mutex, not of the engine, so it is safe inside `emit`.
#[derive(Default)]
pub struct PreviewGate {
    ingress: Mutex<Option<Ingress>>,
}

impl PreviewGate {
    /// A preview is starting: from now on chunks carrying this id are wanted.
    pub fn open(&self, request_id: &str) {
        self.close();
        let (sender, receiver) = tokio::sync::mpsc::channel(SPEECH_QUEUE_CHUNKS);
        let (stopped, _) = tokio::sync::watch::channel(false);
        if let Ok(mut slot) = self.ingress.lock() {
            *slot = Some(Ingress {
                request_id: request_id.to_string(),
                sender: Some(sender),
                receiver: Some(receiver),
                cancelled: Arc::new(AtomicBool::new(false)),
                stopped,
            });
        }
    }

    /// The preview ended, was cancelled, or its playback died.
    pub fn close(&self) {
        if let Ok(mut slot) = self.ingress.lock() {
            if let Some(ingress) = slot.take() {
                ingress.cancelled.store(true, Ordering::SeqCst);
                let _ = ingress.stopped.send(true);
            }
        }
    }

    #[cfg(test)]
    pub fn is_listening(&self, request_id: &str) -> bool {
        self.ingress
            .lock()
            .map(|slot| {
                slot.as_ref().is_some_and(|ingress| {
                    ingress.request_id == request_id && ingress.sender.is_some()
                })
            })
            .unwrap_or(false)
    }

    pub fn is_current(&self, request_id: &str) -> bool {
        self.ingress
            .lock()
            .map(|slot| {
                slot.as_ref().is_some_and(|ingress| {
                    ingress.request_id == request_id && !ingress.cancelled.load(Ordering::SeqCst)
                })
            })
            .unwrap_or(false)
    }

    pub fn take_receiver(
        &self,
        request_id: &str,
    ) -> Option<(tokio::sync::mpsc::Receiver<Vec<u8>>, Arc<AtomicBool>)> {
        let mut slot = self.ingress.lock().ok()?;
        let ingress = slot.as_mut()?;
        if ingress.request_id != request_id {
            return None;
        }
        Some((ingress.receiver.take()?, Arc::clone(&ingress.cancelled)))
    }

    /// End-of-input is distinct from Stop: the decoder must drain accepted
    /// bytes and finish playback, even if Started is still crossing the UI hop.
    pub fn finish(&self, request_id: &str) {
        if let Ok(mut slot) = self.ingress.lock() {
            if let Some(ingress) = slot.as_mut().filter(|i| i.request_id == request_id) {
                ingress.sender = None;
            }
        }
    }

    pub async fn deliver(&self, chunk: SpeechChunk) -> Result<(), String> {
        if chunk.data.len() > 64 * 1024 {
            return Err("Speech chunk exceeds the native queue limit".to_string());
        }
        let (sender, mut stopped) = self
            .ingress
            .lock()
            .ok()
            .and_then(|slot| {
                slot.as_ref()
                    .filter(|ingress| ingress.request_id == chunk.request_id)
                    .and_then(|ingress| {
                        ingress
                            .sender
                            .clone()
                            .map(|sender| (sender, ingress.stopped.subscribe()))
                    })
            })
            .ok_or_else(|| "The voice preview is no longer listening".to_string())?;
        if *stopped.borrow() {
            return Err("Speech playback cancelled".to_string());
        }
        tokio::select! {
            biased;
            _ = stopped.changed() => Err("Speech playback cancelled".to_string()),
            result = sender.send(chunk.data) => result.map_err(|_| "The voice preview is no longer listening".to_string()),
        }
    }
}

pub struct NativeEvents {
    preview: Arc<PreviewGate>,
}

impl NativeEvents {
    pub fn new(preview: Arc<PreviewGate>) -> Self {
        Self { preview }
    }
}

impl EngineEvents for NativeEvents {
    fn emit(&self, event: EngineEvent) -> Result<(), String> {
        // The one event whose delivery the engine acts on. Refusing here is how
        // a cancelled preview stops the download.
        if matches!(event, EngineEvent::TtsChunk(_)) {
            return Err("Native speech audio requires async delivery".to_string());
        }
        if let EngineEvent::TtsFinished(result) = &event {
            self.preview.finish(&result.request_id);
        }

        DispatchQueue::main().exec_async(move || {
            crate::app::with_app(|app| app.handle_event(event));
        });
        Ok(())
    }

    fn emit_speech_chunk(
        &self,
        chunk: SpeechChunk,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(self.preview.deliver(chunk))
    }
}

/// Run `body` on the main thread on a later run-loop turn. Used by hotkey and
/// background callbacks; never reenters the engine while its locks are held.
pub fn on_main<F: FnOnce() + Send + 'static>(body: F) {
    DispatchQueue::main().exec_async(body);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(id: &str, sequence: u64) -> SpeechChunk {
        SpeechChunk {
            request_id: id.to_string(),
            sequence,
            data: vec![42; 64 * 1024],
        }
    }

    #[test]
    fn audio_backpressure_yields_and_stop_wakes_a_full_queue() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let gate = PreviewGate::default();
                gate.open("slow");
                // Keep the receiver alive but do not decode. Stop still has to
                // release a blocked producer, even if opening the device stalled.
                let (_receiver, _) = gate.take_receiver("slow").unwrap();
                for sequence in 0..SPEECH_QUEUE_CHUNKS {
                    gate.deliver(chunk("slow", sequence as u64)).await.unwrap();
                }
                let waiting = gate.deliver(chunk("slow", 4));
                tokio::pin!(waiting);
                tokio::select! {
                    result = &mut waiting => panic!("a full queue accepted more audio: {result:?}"),
                    _ = tokio::time::sleep(std::time::Duration::from_millis(20)) => {},
                }
                gate.close();
                assert!(
                    tokio::time::timeout(std::time::Duration::from_millis(100), waiting)
                        .await
                        .unwrap()
                        .is_err()
                );
            });
    }

    #[test]
    fn a_download_can_finish_before_started_reaches_appkit() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let gate = PreviewGate::default();
                gate.open("early");
                gate.deliver(chunk("early", 0)).await.unwrap();
                gate.finish("early");
                assert!(gate.is_current("early"));
                assert!(!gate.is_listening("early"));
                let (mut receiver, cancelled) = gate.take_receiver("early").unwrap();
                assert_eq!(receiver.recv().await.unwrap().len(), 64 * 1024);
                assert!(receiver.recv().await.is_none());
                assert!(!cancelled.load(Ordering::SeqCst));
                gate.open("new");
                gate.finish("early");
                assert!(gate.is_listening("new"));
            });
    }
}
