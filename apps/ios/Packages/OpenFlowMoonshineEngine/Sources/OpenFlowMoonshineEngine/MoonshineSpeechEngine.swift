import Foundation
import MoonshineVoice
import OpenFlowMobileCore

/// The recogniser. Moonshine base-en or tiny-en, on the CPU, behind the app's
/// one `SpeechEngine` seam.
///
/// Decided in `docs/mobile/M2-MOONSHINE.md` after the desktop benchmark: 62M and
/// 27M parameters against Qwen3-ASR-0.6B's 600M, 141 MB and 44 MB of weights
/// against about 700 MB, a fifth of a second to load from disk against three
/// seconds, and on the reference clip it spells the product names as well as a
/// model three times its size. It runs on the CPU because that is where it was
/// measured to run; a Core ML execution provider is a later opt-in measurement,
/// never a silent change.
///
/// **This engine is the one-unbroken-stretch kind** the `SpeechEngine` doc
/// comment describes. `transcribeWithoutStreaming` is a single synchronous call
/// into a C++ static library with no suspension point inside it, so an `unload()`
/// arriving during a take is queued behind that take by actor serialisation and
/// runs when it finishes. That is what makes `ModelManager.handleMemoryWarning()`
/// safe to call without warning, and it is why this engine needs no defence of
/// its own against being torn down mid-recognition.
public actor MoonshineSpeechEngine: SpeechEngine {

    /// "moonshine-base-en" or "moonshine-tiny-en", for the diagnostics screen.
    public nonisolated let identifier: String

    private let choice: EngineChoice
    private let modelArch: ModelArch

    /// Where the weights are read from. Public so the diagnostics screen can
    /// show the path a failed load names, and so a test can check that the
    /// engine and the downloader agree on it: they have to, or the app fetches
    /// 141 MB into a directory nothing opens.
    public nonisolated let modelDirectory: URL

    /// The three files this engine needs, by name. `Transcriber` opens the
    /// directory, not the files, so this exists to give a missing-weights
    /// failure a name before the C++ library gets a chance to fail vaguely.
    private static let requiredFiles = ["encoder_model.ort", "decoder_model_merged.ort", "tokenizer.bin"]

    private var transcriber: Transcriber?
    private var footprintDelta: Int = 0

    /// The terms offered to the recogniser on the next take. Read-only from
    /// outside; set it with `setKeyterms`.
    public private(set) var keyterms: [String] = []

    /// How many times the weights have actually been brought into memory.
    ///
    /// Two jobs. It is what the diagnostics screen shows next to the load and
    /// unload timestamps PLAN.md section 2 asks for, and it is what makes
    /// `load()`'s idempotence testable: a guard that quietly stopped working
    /// would double the memory and change nothing else observable.
    public private(set) var loadCount = 0

    /// The engine for a choice, reading weights out of the app's model store.
    /// This is the initialiser the app uses.
    public init(choice: EngineChoice, store: ModelStore) {
        self.init(choice: choice, modelDirectory: store.subdirectory(Self.subdirectory(for: choice)).directory)
    }

    /// The engine for a choice, reading weights out of a directory named
    /// directly. What the tests use, and what a diagnostics build can point at a
    /// model somebody put on the device by hand.
    public init(choice: EngineChoice, modelDirectory: URL) {
        self.choice = choice
        self.modelDirectory = modelDirectory
        switch choice {
        case .moonshineBase:
            self.identifier = "moonshine-base-en"
            self.modelArch = .base
        case .moonshineTiny:
            self.identifier = "moonshine-tiny-en"
            self.modelArch = .tiny
        }
    }

    /// Where a choice's weights live under the model store, matching the pin the
    /// downloader installs them with.
    private static func subdirectory(for choice: EngineChoice) -> String {
        ModelDownloader.pin(for: choice).subdirectory
    }

    /// The streaming architectures are deliberately not used. On the reference
    /// clip all four misheard the trailing date and were slower per offline
    /// take, and a dictation app transcribes a finished take rather than a live
    /// stream (`M2-MOONSHINE.md`, "The package").
    public nonisolated var architecture: ModelArch { modelArch }

    public var isLoaded: Bool { transcriber != nil }

    /// Measured, not guessed: the process footprint after `load()` minus the
    /// footprint before it, floored at the size of the weights on disk.
    ///
    /// It is a **delta**, and that is a real limitation to state rather than
    /// paper over. It attributes to the model every page the process gained
    /// while the model was loading, and it misses anything the allocator had
    /// already reserved. What it is honest about is the direction and the order
    /// of magnitude, which is what the Settings screen needs: the cost of having
    /// the model in memory, as the system counts it.
    ///
    /// The floor is there because a delta can come out absurdly low if the
    /// allocator satisfied the load out of pages it was already holding, and
    /// "the model costs 4 MB" is a worse answer than "at least the 141 MB it
    /// occupies on disk".
    public var residentBytes: Int { transcriber == nil ? 0 : footprintDelta }

    /// The terms to bias the recogniser towards on the next take, taken from the
    /// user's dictionary.
    ///
    /// Set from `SettingsStore.dictionary` before a take. The entries are parsed
    /// by `DictionaryPostPass`, so a `heard -> Correct` rule offers the engine
    /// the spelling the user wants to see rather than the mishearing, and
    /// commas cannot reach Moonshine's comma-delimited list because the
    /// dictionary already splits on them.
    ///
    /// This is a hint, not the mechanism. Moonshine applies key terms only on
    /// its streaming architectures, so on base-en and tiny-en the call is
    /// refused and the terms have no effect at all. `DictionaryPostPass` runs
    /// over the finished transcript either way and is what actually guarantees
    /// the spelling, the same belt-and-braces the desktop uses. The call stays
    /// because it costs nothing and starts working by itself the day a
    /// streaming architecture earns its place.
    public func setKeyterms(_ terms: [String]) {
        keyterms = terms.filter { !$0.isEmpty && !$0.contains(",") }
    }

    /// The same thing from the raw dictionary field, which is what the app has.
    public func setKeyterms(fromDictionary dictionary: String?) {
        setKeyterms(DictionaryPostPass.entries(from: dictionary).map(\.replacement))
    }

    /// Bring the weights into memory. Idempotent: a second call while loaded
    /// returns without constructing a second `Transcriber`.
    ///
    /// The guard is not a nicety. `ModelManager` prewarms on capture start and
    /// again on the Action Button intent (PLAN.md section 2), so two loads
    /// racing for the same take is the ordinary case, not the exotic one, and
    /// without the guard the second one would allocate a second copy of the
    /// weights and leave the first with nothing holding it but a dropped
    /// reference.
    public func load() async throws {
        if transcriber != nil { return }

        for name in Self.requiredFiles {
            let file = modelDirectory.appendingPathComponent(name)
            guard FileManager.default.fileExists(atPath: file.path) else {
                throw SpeechEngineError.modelUnavailable(
                    "\(identifier) is missing \(name) in \(modelDirectory.path)"
                )
            }
        }

        let before = ProcessFootprint.bytes()
        let loaded: Transcriber
        do {
            loaded = try Transcriber(modelPath: modelDirectory.path, modelArch: modelArch)
        } catch {
            throw SpeechEngineError.loadFailed("\(identifier): \(error)")
        }
        let after = ProcessFootprint.bytes()

        transcriber = loaded
        loadCount += 1
        let floor = weightsBytesOnDisk()
        if let before, let after {
            footprintDelta = max(after - before, floor)
        } else {
            footprintDelta = floor
        }
    }

    /// Drop the weights. Idempotent, and does not throw.
    ///
    /// `close()` frees the C++ side. `Transcriber.deinit` also calls it, so the
    /// call here is what makes the timing ours rather than ARC's: a memory
    /// warning wants the pages back now, not whenever the last reference
    /// happens to go.
    public func unload() async {
        guard let transcriber else { return }
        transcriber.close()
        self.transcriber = nil
        footprintDelta = 0
    }

    /// Recognise 16 kHz mono Float32 samples in [-1, 1].
    ///
    /// The transcript is `lines[].text` joined with a space and trimmed. An
    /// empty result throws `noSpeechRecognised` rather than returning an empty
    /// string, the same rule the desktop applies: a take that recognised nothing
    /// is something the sheet has to say out loud, not an empty clipboard the
    /// user discovers in another app.
    public func transcribe(samples16k: [Float]) async throws -> OpenFlowMobileCore.Transcript {
        guard let transcriber else {
            throw SpeechEngineError.loadFailed("\(identifier): transcribe called while unloaded")
        }

        if !keyterms.isEmpty {
            // Refused on the non-streaming architectures this engine loads. The
            // post-pass is what guarantees the spelling, so a refusal here is
            // not a reason to fail the user's take.
            try? transcriber.setKeyterms(keyterms)
        }

        let started = DispatchTime.now().uptimeNanoseconds
        let result: MoonshineVoice.Transcript
        do {
            result = try transcriber.transcribeWithoutStreaming(audioData: samples16k)
        } catch {
            throw SpeechEngineError.transcriptionFailed("\(identifier): \(error)")
        }
        let elapsed = Double(DispatchTime.now().uptimeNanoseconds - started) / 1_000_000_000

        let text = result.lines
            .map(\.text)
            .joined(separator: " ")
            .trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty else { throw SpeechEngineError.noSpeechRecognised }

        return OpenFlowMobileCore.Transcript(text: text, latencySeconds: elapsed)
    }

    /// The weights as they sit on disk, which is the floor the footprint delta
    /// is held to. Falls back to the pin's expected size if the files cannot be
    /// stat'ed, so the floor is never zero.
    private func weightsBytesOnDisk() -> Int {
        var total = 0
        for name in Self.requiredFiles {
            let path = modelDirectory.appendingPathComponent(name).path
            let attributes = try? FileManager.default.attributesOfItem(atPath: path)
            total += ((attributes?[.size] as? NSNumber)?.intValue ?? 0)
        }
        guard total > 0 else { return Int(ModelDownloader.pin(for: choice).expectedBytes) }
        return total
    }
}
