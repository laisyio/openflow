import Foundation

/// A `SpeechEngine` that recognises nothing and says so cheerfully.
///
/// Two jobs. In the tests it is the controllable engine every `ModelManager`
/// case is driven through: load latency, transcribe latency and failure are all
/// dialled from outside. In the app it is what `-D OPENFLOW_FAKE_ENGINE` builds
/// against, so the whole product -- sheet, history, keyboard, Live Activity --
/// can be exercised in the Simulator before a single weight exists (PLAN.md
/// section 6).
public actor FakeEngine: SpeechEngine {
    public nonisolated let identifier = "fake"

    /// The default resident figure is Moonshine base-en's order of magnitude
    /// (`EngineProfile`), not a round gigabyte: a Simulator build reading a cost
    /// four times the real one out of the Settings footer would send somebody
    /// looking for a memory problem that does not exist.
    ///
    /// Injected so a test can make a load take as long as it needs to without
    /// any real waiting. Nil means "return immediately".
    private let clock: IdleClock?
    private var loadSeconds: Double
    private var transcribeSeconds: Double
    private var loadError: SpeechEngineError?
    private var transcribeError: SpeechEngineError?
    private var simulatedResidentBytes: Int
    private var loaded = false

    public private(set) var loadCount = 0
    public private(set) var unloadCount = 0
    public private(set) var transcribeCount = 0

    /// What `transcribe` returns. Defaults to something that visibly is not real
    /// recognition, so a fake build is never mistaken for a working one.
    public var cannedText = "This is the FakeEngine. No model is loaded."

    public init(
        clock: IdleClock? = nil,
        loadSeconds: Double = 0,
        transcribeSeconds: Double = 0,
        residentBytes: Int = 420_000_000
    ) {
        self.clock = clock
        self.loadSeconds = loadSeconds
        self.transcribeSeconds = transcribeSeconds
        self.simulatedResidentBytes = residentBytes
    }

    public var residentBytes: Int { loaded ? simulatedResidentBytes : 0 }
    public var isLoaded: Bool { loaded }

    public func setLoadError(_ error: SpeechEngineError?) { loadError = error }
    public func setTranscribeError(_ error: SpeechEngineError?) { transcribeError = error }
    public func setLoadSeconds(_ seconds: Double) { loadSeconds = seconds }
    public func setTranscribeSeconds(_ seconds: Double) { transcribeSeconds = seconds }
    public func setCannedText(_ text: String) { cannedText = text }

    public func load() async throws {
        if loaded { return }
        loadCount += 1
        if loadSeconds > 0, let clock { await clock.sleep(seconds: loadSeconds) }
        if let loadError {
            loaded = false
            throw loadError
        }
        loaded = true
    }

    public func unload() async {
        guard loaded else { return }
        unloadCount += 1
        loaded = false
    }

    public func transcribe(samples16k: [Float]) async throws -> Transcript {
        transcribeCount += 1
        guard loaded else { throw SpeechEngineError.loadFailed("transcribe called while unloaded") }
        if transcribeSeconds > 0, let clock { await clock.sleep(seconds: transcribeSeconds) }
        if let transcribeError { throw transcribeError }
        return Transcript(
            text: cannedText,
            latencySeconds: transcribeSeconds
        )
    }
}
