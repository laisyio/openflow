/** Exactly one speech session owns audio, callbacks and object URLs at a time. */
export interface SpeechChunk { request_id: string; sequence: number; data_base64: string }
export interface SpeechCompletion { request_id: string; bytes: number; mime_type?: string }
export interface PlaybackCallbacks {
  audio: () => HTMLAudioElement | null;
  url: (value: string) => void;
  state: (value: "idle" | "streaming" | "ready" | "error") => void;
  error: (value: string) => void;
  cancel: (id: string) => void;
}
interface Session {
  id: string; mime: string; bytes: number; chunks: Map<number, Uint8Array>;
  source: MediaSource | null; buffer: SourceBuffer | null; next: number;
  complete: boolean; started: boolean; disposed: boolean; url: string;
  expected: SpeechCompletion | null; completionTimer: ReturnType<typeof setTimeout> | null;
  attachmentTimer: ReturnType<typeof setTimeout> | null;
  drainTimer: ReturnType<typeof setTimeout> | null; appendPending: boolean;
  listeners: Array<() => void>;
}
const MAX_BYTES = 50 * 1024 * 1024;
const MAX_CHUNKS = 65536; // Bound Map/typed-array overhead as well as audio bytes.
export class SpeechPlayback {
  private session: Session | null = null;
  constructor(private callbacks: PlaybackCallbacks, private limit = MAX_BYTES,
    private completionTimeoutMs = 5000, private drainTimeoutMs = 10000) {}
  get requestId(): string | null { return this.session?.complete ? null : this.session?.id ?? null; }
  get retainedBytes(): number { return this.session?.bytes ?? 0; }

  start(id: string, mime: string): void {
    this.dispose();
    const s: Session = { id, mime, bytes: 0, chunks: new Map(), source: null, buffer: null,
      next: 0, complete: false, started: false, disposed: false, url: "", listeners: [],
      expected: null, completionTimer: null, attachmentTimer: null, drainTimer: null, appendPending: false };
    this.session = s;
    this.callbacks.error(""); this.callbacks.state("streaming");
    if (mime !== "audio/mpeg" || typeof MediaSource === "undefined" || !MediaSource.isTypeSupported(mime)) return;
    s.source = new MediaSource();
    s.url = URL.createObjectURL(s.source); this.callbacks.url(s.url);
    this.on(s, s.source, "sourceclose", () => this.fallback(s));
    this.on(s, s.source, "sourceopen", () => {
      if (!this.current(s)) return;
      try {
        s.buffer = s.source!.addSourceBuffer(mime); s.buffer.mode = "sequence";
        this.on(s, s.buffer, "updateend", () => {
          if (!this.current(s) || !s.buffer || s.buffer.updating || !s.appendPending) return;
          s.appendPending = false;
          this.pump(s);
          // Only a completed append is progress. Spurious events or additional
          // bridge callbacks must not extend the post-completion drain budget.
          this.watchDrain(s, true);
        });
        this.on(s, s.buffer, "error", () => this.fallback(s));
        this.pump(s);
        this.watchDrain(s);
      } catch { this.fallback(s); }
    });
  }
  chunk(chunk: SpeechChunk): void {
    const s = this.session;
    if (!s || s.complete || chunk.request_id !== s.id) return;
    if (!Number.isSafeInteger(chunk.sequence) || chunk.sequence < 0 || chunk.sequence >= MAX_CHUNKS || s.chunks.has(chunk.sequence)) {
      this.fail(s.id, "Invalid or duplicate speech audio sequence."); return;
    }
    try {
      // Reject enormous bridge payloads before atob allocates their decoded form.
      if (chunk.data_base64.length > Math.ceil((this.limit - s.bytes) / 3) * 4) throw new Error("Speech audio exceeds the playback limit.");
      const raw = atob(chunk.data_base64);
      if (!raw.length || s.bytes + raw.length > this.limit) throw new Error("Speech audio exceeds the playback limit.");
      const bytes = Uint8Array.from(raw, value => value.charCodeAt(0));
      s.bytes += bytes.length; s.chunks.set(chunk.sequence, bytes); this.pump(s);
      if (s.expected) this.finish(s.expected);
    } catch (error) { this.fail(s.id, String(error)); }
  }
  finish(result: SpeechCompletion): void {
    const s = this.session;
    if (!s || s.complete || result.request_id !== s.id) return;
    // Events and command replies travel through different bridge callbacks.
    // Completion may overtake chunks; wait a bounded interval instead of
    // discarding otherwise valid audio or keeping an incomplete stream forever.
    if (!Number.isSafeInteger(result.bytes) || result.bytes <= 0 || result.bytes > this.limit ||
        s.bytes > result.bytes || (s.expected && s.expected.bytes !== result.bytes)) {
      this.fail(s.id, "Voice preview failed. Audio was empty or incomplete."); return;
    }
    s.expected = result;
    if (s.bytes < result.bytes) {
      s.completionTimer ??= setTimeout(() => {
        if (this.current(s) && !s.complete) this.fail(s.id, "Voice preview failed. Audio delivery timed out.");
      }, this.completionTimeoutMs);
      return;
    }
    if ([...s.chunks.keys()].some((_, i) => !s.chunks.has(i))) {
      this.fail(s.id, "Voice preview failed. Audio was incomplete."); return;
    }
    if (s.completionTimer) clearTimeout(s.completionTimer);
    s.completionTimer = null;
    s.complete = true;
    s.mime = result.mime_type || s.mime;
    if (s.source) {
      this.pump(s);
      this.watchDrain(s);
      // A removed/unattached audio element may never emit sourceopen OR
      // sourceclose. Keep successful completion bounded in that case too.
      if (s.source && !s.buffer) s.attachmentTimer = setTimeout(() => {
        if (this.current(s) && !s.buffer) this.fallback(s);
      }, this.completionTimeoutMs);
    } else this.blob(s);
    this.callbacks.state("ready");
  }
  fail(id: string, message: string): void {
    if (this.session?.id !== id || this.session.complete) return;
    this.dispose(); this.callbacks.error(message); this.callbacks.state("error");
  }
  dispose(): void {
    const s = this.session;
    this.session = null; // Invalidate before cancellation can deliver callbacks.
    if (s) {
      s.disposed = true;
      if (s.completionTimer) clearTimeout(s.completionTimer);
      if (!s.complete) this.callbacks.cancel(s.id);
      this.callbacks.audio()?.pause();
      this.detach(s);
      s.chunks.clear(); s.bytes = 0;
      if (s.url) URL.revokeObjectURL(s.url);
    }
    this.callbacks.url(""); this.callbacks.state("idle");
  }
  private current(s: Session): boolean { return this.session === s && !s.disposed; }
  private on(s: Session, target: EventTarget, name: string, fn: () => void): void {
    target.addEventListener(name, fn); s.listeners.push(() => target.removeEventListener(name, fn));
  }
  private detach(s: Session): void {
    if (s.attachmentTimer) clearTimeout(s.attachmentTimer);
    s.attachmentTimer = null;
    if (s.drainTimer) clearTimeout(s.drainTimer);
    s.drainTimer = null; s.appendPending = false;
    for (const remove of s.listeners.splice(0)) remove();
    try { if (s.buffer?.updating) s.buffer.abort(); } catch { /* Already detached. */ }
    s.buffer = null; s.source = null;
  }
  private watchDrain(s: Session, progress = false): void {
    if (!this.current(s) || !s.complete || !s.source || !s.buffer) return;
    if (s.drainTimer) {
      if (!progress) return;
      clearTimeout(s.drainTimer);
    }
    // Receiving every byte does not guarantee a SourceBuffer will ever finish
    // its pending append. Retain replay, but release the duplicate chunk map.
    s.drainTimer = setTimeout(() => {
      if (this.current(s) && s.complete && s.buffer) this.fallback(s);
    }, this.drainTimeoutMs);
  }
  private pump(s: Session): void {
    if (!this.current(s) || !s.buffer || s.buffer.updating || s.source?.readyState !== "open") return;
    try {
      if (!s.started && s.buffer.buffered.length > 0) {
        s.started = true;
        void this.callbacks.audio()?.play().catch(() => {
          if (this.current(s)) this.callbacks.error("Live playback was blocked. Use the audio controls to continue.");
        });
      }
      const next = s.chunks.get(s.next);
      if (next) { s.next++; s.appendPending = true; s.buffer.appendBuffer(next as Uint8Array<ArrayBuffer>); }
      else if (s.complete) {
        s.source.endOfStream();
        // The browser owns the finished MediaSource for replay. The application
        // no longer retains an additional copy or completion listeners.
        s.chunks.clear(); s.bytes = 0; this.detach(s);
      }
    } catch { this.fallback(s); }
  }
  private fallback(s: Session): void {
    if (!this.current(s)) return;
    this.detach(s);
    if (s.complete) this.blob(s);
  }
  private blob(s: Session): void {
    const parts = [...s.chunks.entries()].sort(([a], [b]) => a - b).map(([, b]) => b as Uint8Array<ArrayBuffer>);
    const url = URL.createObjectURL(new Blob(parts, { type: s.mime }));
    if (s.url) URL.revokeObjectURL(s.url);
    s.url = url; s.chunks.clear(); s.bytes = 0; this.callbacks.url(url);
  }
}
