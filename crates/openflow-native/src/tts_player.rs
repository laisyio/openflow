//! Playback for the voice preview.
//!
//! The webview used a `MediaSource`; here the same job is `rodio` on a private
//! thread. Two shapes, because the providers differ:
//!
//! - **mp3 and friends**: a channel-backed reader is handed to the decoder the
//!   moment the stream starts, so the first audio plays while the rest is still
//!   downloading. Reads past what has arrived block until the next chunk does.
//! - **WAV** (Groq's Orpheus answers only in WAV): spooled to an anonymous file and
//!   played when the stream finishes. A WAV header describes a length, and a
//!   half-written one is not a clip.
//!
//! Cancellation is a flag the source checks: it reports end-of-file, the
//! decoder finishes, and the player thread exits. Both shapes are wrapped in
//! [`StopsWhenCancelled`] on the way to the decoder, because neither one stops
//! on its own -- see that type. The same flag closes the [`PreviewGate`], which
//! is what makes the *engine* stop downloading, since `speech::stream` gives up
//! when a chunk cannot be delivered.

use std::cell::RefCell;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::Receiver;

use openflow_core::speech::{SpeechError, SpeechResult, SpeechStarted};

use crate::events::PreviewGate;

/// A `Read + Seek` view over bytes that are still arriving.
///
/// Symphonia probes the container before it decodes, and probing seeks. So this
/// spools received bytes to an anonymous file rather than retaining the clip
/// on the heap. No pathname survives creation. Seeking
/// backwards is free, seeking forwards pulls more, and reading past the end
/// blocks until the next chunk lands or the stream ends.
struct StreamBuffer {
    /// Behind a mutex only to satisfy the decoder's `Sync` bound: exactly one
    /// thread ever reads it.
    chunks: Mutex<Receiver<Vec<u8>>>,
    data: File,
    length: u64,
    position: u64,
    finished: bool,
    cancelled: Arc<AtomicBool>,
}

impl StreamBuffer {
    fn new(chunks: Receiver<Vec<u8>>, cancelled: Arc<AtomicBool>) -> io::Result<Self> {
        // Unlink before putting audio in it: no named speech file can survive
        // a crash, and the kernel releases the storage when the reader drops.
        let path = std::env::temp_dir().join(format!("openflow-speech-{}", uuid::Uuid::new_v4()));
        let data = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        std::fs::remove_file(&path)?;
        Ok(Self {
            chunks: Mutex::new(chunks),
            data,
            length: 0,
            position: 0,
            finished: false,
            cancelled,
        })
    }

    /// Pull until `wanted` bytes exist, the stream ends, or playback is
    /// cancelled. Returns whether the target was reached.
    fn fill_to(&mut self, wanted: u64) -> io::Result<bool> {
        while self.length < wanted && !self.finished {
            if self.cancelled.load(Ordering::SeqCst) {
                self.finished = true;
                return Ok(false);
            }
            let next = self.chunks.lock().map(|mut chunks| chunks.blocking_recv());
            match next {
                Ok(Some(chunk)) => {
                    self.data.seek(SeekFrom::Start(self.length))?;
                    self.data.write_all(&chunk)?;
                    self.length += chunk.len() as u64;
                }
                _ => self.finished = true,
            }
        }
        Ok(self.length >= wanted)
    }
}

impl Read for StreamBuffer {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let start = self.position;
        self.fill_to(start.saturating_add(1))?;
        if start >= self.length {
            return Ok(0);
        }
        let take = (self.length - start).min(out.len() as u64) as usize;
        self.data.seek(SeekFrom::Start(start))?;
        self.data.read_exact(&mut out[..take])?;
        self.position += take as u64;
        Ok(take)
    }
}

impl Seek for StreamBuffer {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let target = match from {
            SeekFrom::Start(offset) => offset as i64,
            SeekFrom::Current(delta) => self.position as i64 + delta,
            SeekFrom::End(delta) => {
                // The only way to know the end is to have all of it.
                self.fill_to(u64::MAX)?;
                self.length as i64 + delta
            }
        };
        if target < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot seek before the start of the stream",
            ));
        }
        self.position = target as u64;
        Ok(self.position)
    }
}

/// Reports end-of-file the moment playback is cancelled, whatever it wraps.
///
/// [`StreamBuffer`] already consults the same flag, but only where it would
/// otherwise block waiting for a chunk that will never come; bytes it has
/// already pulled are still served, and the buffered WAV path is a plain
/// `Cursor` that has never heard of the flag at all. So Stop closed the gate,
/// dropped the sender and left the speaker playing. Measured on a silent
/// 8-second WAV, decoding without a device: after the flag was set the decoder
/// still handed out 351_800 of 352_800 samples through the `Cursor`, and 31_746
/// through `StreamBuffer` -- one symphonia read-ahead buffer, which for a
/// 128 kbps mp3 is about four seconds of audio.
///
/// Wrapping the source is where the fix belongs: it is the one place both
/// shapes pass through, and it also releases the retained clip as soon as the
/// player thread unwinds rather than at the end of a preview nobody wanted.
struct StopsWhenCancelled<R> {
    inner: R,
    cancelled: Arc<AtomicBool>,
}

impl<R> StopsWhenCancelled<R> {
    fn new(inner: R, cancelled: Arc<AtomicBool>) -> Self {
        Self { inner, cancelled }
    }
}

impl<R: Read> Read for StopsWhenCancelled<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.cancelled.load(Ordering::SeqCst) {
            return Ok(0);
        }
        self.inner.read(out)
    }
}

impl<R: Seek> Seek for StopsWhenCancelled<R> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        self.inner.seek(from)
    }
}

/// Exactly what [`spawn_player`] hands the decoder, named so the tests can
/// decode the same thing the speaker would. Both shapes of preview go through
/// here: the incrementally filled reader and the finished WAV spool.
fn player_source<R>(source: R, cancelled: Arc<AtomicBool>) -> impl Read + Seek + Send + Sync
where
    R: Read + Seek + Send + Sync + 'static,
{
    StopsWhenCancelled::new(source, cancelled)
}

/// One preview in flight.
struct Playback {
    request_id: String,
    cancelled: Arc<AtomicBool>,
}

#[derive(Default)]
struct PlaybackErrors {
    request_id: String,
    error: Option<String>,
}

pub struct TtsPlayer {
    preview: Arc<PreviewGate>,
    playback: RefCell<Option<Playback>>,
    /// The last error a player thread hit, for the settings window to show.
    last_error: Arc<Mutex<PlaybackErrors>>,
}

impl TtsPlayer {
    pub fn new(preview: Arc<PreviewGate>) -> Self {
        Self {
            preview,
            playback: RefCell::new(None),
            last_error: Arc::new(Mutex::new(PlaybackErrors::default())),
        }
    }

    /// The last failure a player thread reported, for the settings window.
    pub fn last_error(&self) -> Option<String> {
        self.last_error
            .lock()
            .ok()
            .and_then(|slot| slot.error.clone())
    }

    /// Arm the gate for a preview that is about to be requested.
    ///
    /// This runs *before* the stream is spawned, and it has to: `speech::stream`
    /// emits `TtsStarted` and then the first chunk from a tokio worker, while
    /// this host only learns about `TtsStarted` after a main-queue hop. Opening
    /// the gate in `started` would lose that race whenever the first chunk
    /// arrives before the hop runs -- a local TTS endpoint, or a busy main
    /// thread -- and `emit` would refuse the chunk, killing the stream with
    /// "Could not deliver speech audio".
    pub fn arm(&self, request_id: &str) {
        self.stop_playback();
        if let Ok(mut errors) = self.last_error.lock() {
            *errors = PlaybackErrors {
                request_id: request_id.to_string(),
                error: None,
            };
        }
        self.preview.open(request_id);
    }

    /// A stream is starting. For a streamable container the decoder starts
    /// straight away.
    ///
    /// The gate decides whether this event is still wanted, and it is the only
    /// thing that does. Every preview passes through [`Self::arm`] first, so an
    /// id the gate is not holding is an event that has been overtaken: Stop was
    /// pressed while `TtsStarted` was crossing the main-queue hop, or a second
    /// Preview armed a newer request. Acting on either would restart a preview
    /// the user already dismissed, or hijack the newer one's playback.
    pub fn started(&self, started: &SpeechStarted) {
        if !self.preview.is_current(&started.request_id) {
            return;
        }

        let Some((receiver, cancelled)) = self.preview.take_receiver(&started.request_id) else {
            return;
        };
        let buffered_only = started.format.eq_ignore_ascii_case("wav");
        spawn_player(
            receiver,
            buffered_only,
            Arc::clone(&cancelled),
            Arc::clone(&self.last_error),
            started.request_id.clone(),
        );

        *self.playback.borrow_mut() = Some(Playback {
            request_id: started.request_id.clone(),
            cancelled,
        });
    }

    /// The stream ended cleanly. A streaming preview just gets its end-of-file;
    /// a buffered one starts playing now.
    pub fn finished(&self, result: &SpeechResult) {
        self.preview.finish(&result.request_id);
    }

    /// A stream failed or was cancelled.
    ///
    /// Guarded on the id for the same reason [`Self::finished`] is. One id per
    /// preview means a cancelled stream reports its cancellation *after* the
    /// next preview has been armed; closing the gate on that would stop the new
    /// preview and leave the old one's message on screen.
    pub fn failed(&self, error: &SpeechError) {
        if !self.is_current(&error.request_id) {
            return;
        }
        if let Ok(mut slot) = self.last_error.lock() {
            slot.error = Some(error.error.clone());
        }
        self.stop();
    }

    /// Whether `request_id` is the preview this player is serving: the one
    /// playing, or, before `TtsStarted` has arrived, the one the gate is armed
    /// for.
    pub fn is_current(&self, request_id: &str) -> bool {
        match self.playback.borrow().as_ref() {
            Some(playback) => playback.request_id == request_id,
            None => self.preview.is_current(request_id),
        }
    }

    /// Stop whatever is playing and stop accepting audio for it.
    pub fn stop(&self) {
        self.stop_playback();
        self.preview.close();
    }

    /// Stop the player without touching the gate, for the paths that are about
    /// to open it again for a new request.
    fn stop_playback(&self) {
        if let Some(playback) = self.playback.borrow_mut().take() {
            playback.cancelled.store(true, Ordering::SeqCst);
        }
    }
}

/// Decode and play `source` on its own thread. The audio device is opened
/// there, not on the main thread, so a slow device never stalls the UI.
fn spawn_player(
    receiver: Receiver<Vec<u8>>,
    buffered_only: bool,
    cancelled: Arc<AtomicBool>,
    errors: Arc<Mutex<PlaybackErrors>>,
    request_id: String,
) {
    std::thread::spawn(move || {
        let report = |message: String| {
            if cancelled.load(Ordering::SeqCst) {
                return;
            }
            if let Ok(mut slot) = errors.lock() {
                if slot.request_id != request_id {
                    return;
                }
                slot.error = Some(message.clone());
            }
            let request_id = request_id.clone();
            crate::events::on_main(move || {
                crate::app::with_app(|app| {
                    app.with_settings(|page| {
                        page.set_voice_status_for(&request_id, &message);
                    })
                });
            });
        };
        let mut source = match StreamBuffer::new(receiver, Arc::clone(&cancelled)) {
            Ok(source) => source,
            Err(error) => return report(format!("Could not buffer speech: {error}")),
        };
        if buffered_only {
            if let Err(error) = source.fill_to(u64::MAX) {
                return report(format!("Could not buffer speech: {error}"));
            }
        }
        if cancelled.load(Ordering::SeqCst) {
            return;
        }
        let device = match rodio::DeviceSinkBuilder::open_default_sink() {
            Ok(device) => device,
            Err(error) => return report(format!("No audio output is available: {}", error)),
        };
        let source = player_source(source, Arc::clone(&cancelled));
        let decoder = match rodio::Decoder::new(source) {
            Ok(decoder) => decoder,
            Err(error) => {
                // A cancelled preview reaches the decoder as an empty stream,
                // which is not a failure worth showing the user.
                if !cancelled.load(Ordering::SeqCst) {
                    report(format!("The audio could not be decoded: {}", error));
                }
                return;
            }
        };
        let player = rodio::Player::connect_new(device.mixer());
        player.append(decoder);
        player.sleep_until_end();
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tokio::sync::mpsc::channel;

    #[test]
    fn long_speech_uses_an_unlinked_seekable_spool() {
        use std::os::unix::fs::MetadataExt;
        let (sender, receiver) = channel(4);
        let feeder = std::thread::spawn(move || {
            for _ in 0..160 {
                sender
                    .blocking_send(vec![7; 65_536])
                    .expect("bounded queue");
            }
        });
        let mut source = StreamBuffer::new(receiver, Arc::new(AtomicBool::new(false))).unwrap();
        source.fill_to(u64::MAX).unwrap();
        let metadata = source.data.metadata().unwrap();
        assert_eq!(metadata.len(), 10 * 1024 * 1024);
        assert_eq!(
            metadata.nlink(),
            0,
            "speech must have no recoverable pathname"
        );
        source.seek(SeekFrom::Start(0)).unwrap();
        let mut first = [0; 4];
        source.read_exact(&mut first).unwrap();
        assert_eq!(first, [7; 4]);
        feeder.join().unwrap();
    }

    fn buffer_from(chunks: Vec<&'static [u8]>) -> (StreamBuffer, std::thread::JoinHandle<()>) {
        let (sender, receiver) = channel::<Vec<u8>>(4);
        let cancelled = Arc::new(AtomicBool::new(false));
        let feeder = std::thread::spawn(move || {
            for chunk in chunks {
                if sender.blocking_send(chunk.to_vec()).is_err() {
                    return;
                }
            }
        });
        (
            StreamBuffer::new(receiver, cancelled).expect("anonymous spool"),
            feeder,
        )
    }

    #[test]
    fn mp3_decoding_starts_before_the_download_finishes() {
        // MPEG-1 Layer III, 128 kbps, 44.1 kHz mono. Zero side information
        // encodes silence without a bit reservoir; no audio asset or device.
        let mut frame = vec![0; 417];
        frame[..4].copy_from_slice(&[0xff, 0xfb, 0x90, 0xc4]);
        let audio = frame.repeat(100);
        let (sender, receiver) = channel(4);
        sender.blocking_send(audio).unwrap();
        let (played, result) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let cancelled = Arc::new(AtomicBool::new(false));
            let source = player_source(
                StreamBuffer::new(receiver, Arc::clone(&cancelled)).unwrap(),
                cancelled,
            );
            let decoded = rodio::Decoder::new(source)
                .map(|decoder| decoder.take(1000).count())
                .map_err(|error| error.to_string());
            let _ = played.send(decoded);
        });
        // Keep the producer alive: probing must not seek to EOF and turn MP3
        // playback into a fully buffered download. Close before assertions so
        // a regression can still let the decoder thread exit cleanly.
        let early = result.recv_timeout(std::time::Duration::from_secs(1));
        drop(sender);
        worker.join().unwrap();
        assert_eq!(
            early.expect("MP3 decoder waited for download EOF"),
            Ok(1000)
        );
    }

    /// The decoder reads and seeks over bytes that have not arrived yet. Both
    /// have to work, and reading past the end has to stop rather than hang.
    #[test]
    fn the_channel_reader_serves_bytes_that_arrive_late() {
        let (mut buffer, feeder) = buffer_from(vec![b"ID3\x04", b"hello", b"world"]);

        let mut head = [0u8; 4];
        buffer.read_exact(&mut head).expect("the header arrives");
        assert_eq!(&head, b"ID3\x04");

        // Seek back over bytes already consumed, the way probing does.
        assert_eq!(buffer.seek(SeekFrom::Start(0)).expect("rewind"), 0);
        let mut all = Vec::new();
        buffer.read_to_end(&mut all).expect("drain the stream");
        assert_eq!(all, b"ID3\x04helloworld");

        // Past the end is end-of-file, not a block.
        let mut nothing = [0u8; 8];
        assert_eq!(buffer.read(&mut nothing).expect("eof"), 0);
        feeder.join().expect("the feeder finishes");
    }

    /// Seeking from the end has to know the length, which means draining first.
    #[test]
    fn seeking_from_the_end_drains_the_stream() {
        let (mut buffer, feeder) = buffer_from(vec![b"0123", b"456789"]);
        assert_eq!(buffer.seek(SeekFrom::End(-2)).expect("seek to the tail"), 8);
        let mut tail = Vec::new();
        buffer.read_to_end(&mut tail).expect("read the tail");
        assert_eq!(tail, b"89");
        feeder.join().expect("the feeder finishes");
    }

    /// Cancelling has to unblock a reader waiting on a chunk that will never
    /// come, or the player thread would live for the life of the process.
    #[test]
    fn cancelling_ends_the_stream_for_the_reader() {
        let (sender, receiver) = channel::<Vec<u8>>(4);
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut buffer =
            StreamBuffer::new(receiver, Arc::clone(&cancelled)).expect("anonymous spool");
        sender.blocking_send(b"abc".to_vec()).expect("first chunk");

        let mut head = [0u8; 3];
        buffer.read_exact(&mut head).expect("the first chunk reads");

        cancelled.store(true, Ordering::SeqCst);
        let mut rest = [0u8; 4];
        assert_eq!(
            buffer.read(&mut rest).expect("cancelled reads as eof"),
            0,
            "a cancelled stream must report end-of-file, not wait"
        );
        drop(sender);
    }

    /// A silent 16-bit mono WAV, so that nothing here can make a sound even if
    /// something later hands one of these to a real device. Nothing does: these
    /// tests decode, they never open an output.
    fn silent_wav(samples: u32, rate: u32) -> Vec<u8> {
        let data_len = samples * 2;
        let mut wav = Vec::with_capacity(44 + data_len as usize);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_len).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&1u16.to_le_bytes()); // mono
        wav.extend_from_slice(&rate.to_le_bytes());
        wav.extend_from_slice(&(rate * 2).to_le_bytes()); // bytes per second
        wav.extend_from_slice(&2u16.to_le_bytes()); // block align
        wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        wav.resize(44 + data_len as usize, 0);
        wav
    }

    /// Stop has to stop the sound, and the flag alone did not: `StreamBuffer`
    /// only consults it where it would otherwise block, and the buffered WAV
    /// path is a `Cursor` that never consulted it at all. Decoded here with no
    /// audio device, on silence, so the check costs nothing and plays nothing.
    ///
    /// This crate's tests only ever run on a developer's Mac -- CI is Linux and
    /// compiles the whole thing away -- so this is the only place the promise in
    /// the module header gets checked at all.
    #[test]
    fn cancelling_stops_the_decoder_for_both_shapes_of_preview() {
        let rate = 44_100;
        let total = rate * 8;
        let wav = silent_wav(total, rate);

        // A tenth of a second. The floor is the packet symphonia has already
        // decoded, measured at 514 samples (11.7 ms) for both shapes; the
        // ceiling has to stay under the smaller of the two unfixed numbers,
        // 31_746, or this assertion would wave the bug through.
        let allowed = rate as usize / 10;

        // The buffered shape: Groq's Orpheus answers in WAV, which plays from a
        // `Cursor` over the finished download. Unwrapped it delivered 351_800
        // of 352_800 samples after the flag was set -- every remaining second.
        let cancelled = Arc::new(AtomicBool::new(false));
        let source = player_source(Cursor::new(wav.clone()), Arc::clone(&cancelled));
        let mut decoder = rodio::Decoder::new(source).expect("the silent wav decodes");
        assert_eq!(
            decoder.by_ref().take(1_000).count(),
            1_000,
            "the preview has to play before there is anything to stop"
        );
        cancelled.store(true, Ordering::SeqCst);
        let after = decoder.count();
        assert!(
            after < allowed,
            "a cancelled WAV preview kept playing: {after} samples after Stop, \
             out of {total}"
        );

        // The streaming shape: the same clip arriving in chunks. Unwrapped this
        // one ran on for 31_746 samples -- one read-ahead buffer, which for a
        // 128 kbps mp3 is about four seconds.
        let cancelled = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = channel::<Vec<u8>>(4);
        let feeder = std::thread::spawn(move || {
            for piece in wav.chunks(65_536) {
                if sender.blocking_send(piece.to_vec()).is_err() {
                    break;
                }
            }
        });
        let source = player_source(
            StreamBuffer::new(receiver, Arc::clone(&cancelled)).expect("anonymous spool"),
            Arc::clone(&cancelled),
        );
        let mut decoder = rodio::Decoder::new(source).expect("the silent wav decodes");
        assert_eq!(decoder.by_ref().take(1_000).count(), 1_000);
        cancelled.store(true, Ordering::SeqCst);
        let after = decoder.count();
        feeder.join().expect("feeder exits after cancellation");
        assert!(
            after < allowed,
            "a cancelled streaming preview kept playing: {after} samples after \
             Stop, out of {total}"
        );
    }

    /// The narrow version of the same rule, at the byte level: bytes that have
    /// already been pulled are not a licence to keep playing. This is the case
    /// `cancelling_ends_the_stream_for_the_reader` does not cover -- there the
    /// reader was out of bytes and would have blocked, which is the only place
    /// `StreamBuffer` looks at the flag.
    #[test]
    fn cancelling_stops_a_source_that_still_holds_buffered_bytes() {
        let (sender, receiver) = channel::<Vec<u8>>(4);
        sender
            .blocking_send(b"0123456789".to_vec())
            .expect("the whole clip");
        drop(sender);

        let cancelled = Arc::new(AtomicBool::new(false));
        let mut source = player_source(
            StreamBuffer::new(receiver, Arc::clone(&cancelled)).expect("anonymous spool"),
            Arc::clone(&cancelled),
        );

        let mut head = [0u8; 4];
        source.read_exact(&mut head).expect("playback starts");
        assert_eq!(&head, b"0123");

        cancelled.store(true, Ordering::SeqCst);
        let mut rest = [0u8; 6];
        assert_eq!(
            source.read(&mut rest).expect("cancelled reads as eof"),
            0,
            "a cancelled preview must stop even with the rest of the clip in hand"
        );
    }

    /// Arming before the stream starts is what keeps a chunk that overtakes the
    /// main-queue hop from being refused, so `TtsStarted` must not close and
    /// reopen the gate. The mirror of that rule is that a start event the gate
    /// is not holding is stale and must be ignored.
    #[test]
    fn started_serves_only_the_request_the_gate_is_armed_for() {
        let gate = Arc::new(PreviewGate::default());
        let player = TtsPlayer::new(Arc::clone(&gate));

        player.arm("preview-1");
        assert!(gate.is_listening("preview-1"), "arming opens the gate");

        // WAV, so no audio device is opened: buffered playback starts only on
        // `finished`, which keeps this test off the machine's speakers.
        player.started(&SpeechStarted {
            request_id: "preview-1".to_string(),
            model: "orpheus".to_string(),
            format: "wav".to_string(),
        });
        assert!(
            gate.is_listening("preview-1"),
            "the gate must still be open after the event it was armed for"
        );

        // Stop, then the `TtsStarted` that was already crossing the hop behind
        // it. Reopening the gate here would restart a preview the user has
        // already dismissed.
        player.stop();
        assert!(!gate.is_listening("preview-1"));
        player.started(&SpeechStarted {
            request_id: "preview-1".to_string(),
            model: "orpheus".to_string(),
            format: "wav".to_string(),
        });
        assert!(
            !gate.is_listening("preview-1"),
            "a stopped preview must not be resurrected by its own start event"
        );
        assert!(
            !player.is_current("preview-1"),
            "and no playback may be installed for it"
        );

        // Same rule for a start that arrives after a newer preview was armed:
        // it belongs to nobody, so it must not take the new preview's gate.
        player.arm("preview-2");
        player.started(&SpeechStarted {
            request_id: "preview-1".to_string(),
            model: "orpheus".to_string(),
            format: "wav".to_string(),
        });
        assert!(
            gate.is_listening("preview-2"),
            "a superseded start event must not hijack the live preview"
        );
        assert!(
            player.is_current("preview-2"),
            "the live preview is still the one this player is serving"
        );
        assert!(!player.is_current("preview-1"));

        player.stop();
        assert!(!gate.is_listening("preview-2"));
    }

    /// One id per preview means a cancelled stream reports back *after* the
    /// next preview is armed. An unguarded `failed` closed the gate on that
    /// second preview and killed it, so Preview pressed twice never played.
    #[test]
    fn a_dead_streams_failure_cannot_stop_the_preview_that_replaced_it() {
        let gate = Arc::new(PreviewGate::default());
        let player = TtsPlayer::new(Arc::clone(&gate));

        player.arm("preview-b");
        assert!(gate.is_listening("preview-b"));

        player.failed(&SpeechError {
            request_id: "preview-a".to_string(),
            error: "Speech generation cancelled".to_string(),
            cancelled: true,
        });
        assert!(
            gate.is_listening("preview-b"),
            "the live preview must survive the previous one's cancellation"
        );
        assert_eq!(
            player.last_error(),
            None,
            "and it must not inherit the dead stream's message"
        );

        // The error that does belong to it still lands.
        player.failed(&SpeechError {
            request_id: "preview-b".to_string(),
            error: "No audio output is available".to_string(),
            cancelled: false,
        });
        assert!(!gate.is_listening("preview-b"));
        assert_eq!(
            player.last_error().as_deref(),
            Some("No audio output is available")
        );
    }

    /// The gate is what tells the engine to stop downloading. Closing it has to
    /// make `is_listening` false for the id that was open.
    #[test]
    fn the_gate_tracks_exactly_one_preview() {
        let gate = PreviewGate::default();
        assert!(!gate.is_listening("a"));
        gate.open("a");
        assert!(gate.is_listening("a"));
        assert!(!gate.is_listening("b"));
        gate.open("b");
        assert!(!gate.is_listening("a"));
        assert!(gate.is_listening("b"));
        gate.close();
        assert!(!gate.is_listening("b"));
    }
}
