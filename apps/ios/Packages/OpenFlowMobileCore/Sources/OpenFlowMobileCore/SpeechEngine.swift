import Foundation

/// One recognised take.
public struct Transcript: Sendable, Equatable {
    /// The recognised text, already trimmed. Never empty when the engine
    /// succeeded; an engine that recognised nothing throws instead, the same
    /// rule the desktop applies in `parse_transcription_response`.
    public var text: String
    /// Wall-clock seconds the recognition itself took, for the diagnostics screen.
    public var latencySeconds: Double

    public init(text: String, latencySeconds: Double) {
        self.text = text
        self.latencySeconds = latencySeconds
    }
}

public enum SpeechEngineError: Error, Equatable, Sendable {
    /// Weights are missing or failed their checksum.
    case modelUnavailable(String)
    /// The engine could not bring the weights into memory.
    case loadFailed(String)
    /// Recognition ran but produced nothing usable.
    case noSpeechRecognised
    /// Recognition failed outright.
    case transcriptionFailed(String)
    /// The only accelerator we accept was unavailable.
    ///
    /// Unused by the Moonshine engine, and kept deliberately. The rule it came
    /// from -- never fall back to CPU silently -- was written for a GPU engine;
    /// Moonshine is CPU-native by design, so there is no accelerator for it to
    /// lose (`M2-MOONSHINE.md`, "Why Moonshine"). An engine that does need one,
    /// such as the Qwen accurate option when it returns, throws this rather than
    /// quietly running somewhere it was never measured.
    case acceleratorUnavailable(String)
}

/// The one seam between the app and whatever recognises speech.
///
/// Declared as an `Actor` protocol so an implementation gets its state isolation
/// for free and `ModelManager` can drive it from its own actor without any lock.
/// Milestone M2 fills it in once, with `MoonshineSpeechEngine`.
public protocol SpeechEngine: Actor {
    /// A short identifier for the diagnostics screen, e.g. "moonshine-base-en".
    nonisolated var identifier: String { get }

    /// Bytes the weights occupy right now. Zero when unloaded. Surfaced in
    /// Settings so the memory cost from PLAN.md section 0 is visible, not
    /// implied. An engine that can measure this should measure it rather than
    /// return the size of the weights on disk.
    var residentBytes: Int { get }

    /// Bring the weights into memory. Must be idempotent: calling it while
    /// already loaded is a no-op, not a second allocation.
    func load() async throws

    /// Drop the weights. Must be idempotent and must not throw; the caller is
    /// often reacting to a memory warning and has nowhere to put an error.
    ///
    /// **It must not tear down state a `transcribe(samples16k:)` still running
    /// on this actor is using.** `ModelManager.handleMemoryWarning()` calls this
    /// while a transcription may be in flight -- deliberately, because PLAN.md
    /// section 2 says a memory warning gets no grace period -- and it relies on
    /// actor serialisation to make that safe: the call is queued behind whatever
    /// is executing and runs when the actor is next free.
    ///
    /// That is only a guarantee for an implementation that does its work in one
    /// unbroken stretch. `MoonshineSpeechEngine` is that kind: its recognition
    /// is a single synchronous call into a C++ library
    /// (`transcribeWithoutStreaming`) with no suspension point inside it, so an
    /// unload arriving mid-take is queued behind the take and runs after it, and
    /// the engine needs no defence of its own.
    ///
    /// An engine that suspends *inside* `transcribe` -- awaiting GPU work
    /// between decoder steps, say -- gives this call a window to run in the
    /// middle of a recognition. Such an engine must either hold the weights
    /// alive for the take in progress and free them when it finishes, or cancel
    /// that take and throw `SpeechEngineError.transcriptionFailed`. Freeing
    /// memory out from under a suspended `transcribe` is a crash, not an
    /// optimisation.
    func unload() async

    /// Recognise 16 kHz mono Float32 samples in [-1, 1].
    func transcribe(samples16k: [Float]) async throws -> Transcript
}
