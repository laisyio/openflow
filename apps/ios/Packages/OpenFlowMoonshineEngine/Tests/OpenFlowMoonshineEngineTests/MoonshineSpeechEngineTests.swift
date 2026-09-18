import Foundation
import Testing
@testable import OpenFlowMobileCore
@testable import OpenFlowMoonshineEngine

/// The engine's behaviour that does not need weights: what it is called, where
/// it reads from, and what it does when asked to work without a model. These run
/// everywhere, including where the 141 MB has never been downloaded.
///
/// The tests that need the real model are in `MoonshineRealModelTests` below.
@Suite struct MoonshineSpeechEngineTests {

    // MARK: - Without the model

    /// `identifier` is what the diagnostics screen shows and what a bug report
    /// quotes, so it is pinned rather than derived at the call site.
    @Test func testTheIdentifierNamesTheModelThatWillRun() async {
        let base = MoonshineSpeechEngine(choice: .moonshineBase, modelDirectory: URL(fileURLWithPath: "/nowhere"))
        let tiny = MoonshineSpeechEngine(choice: .moonshineTiny, modelDirectory: URL(fileURLWithPath: "/nowhere"))
        #expect(base.identifier == "moonshine-base-en")
        #expect(tiny.identifier == "moonshine-tiny-en")

        // The offline architectures, not the streaming ones.
        #expect(base.architecture == .base)
        #expect(tiny.architecture == .tiny)
    }

    /// Missing weights are named before the C++ library gets a chance to fail
    /// vaguely, and nothing is reported as resident.
    @Test func testMissingWeightsFailWithTheFileThatIsMissing() async {
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString)
        try? FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        try? Data("not a model".utf8).write(to: directory.appendingPathComponent("encoder_model.ort"))

        let engine = MoonshineSpeechEngine(choice: .moonshineBase, modelDirectory: directory)
        do {
            try await engine.load()
            Issue.record("loading a directory without the weights must throw")
        } catch let error as SpeechEngineError {
            guard case let .modelUnavailable(message) = error else {
                Issue.record("expected modelUnavailable, got \(error)")
                return
            }
            #expect(message.contains("decoder_model_merged.ort"))
            #expect(message.contains("moonshine-base-en"))
        } catch {
            Issue.record("unexpected error \(error)")
        }

        #expect(await engine.residentBytes == 0)
        #expect(await engine.isLoaded == false)
        #expect(await engine.loadCount == 0)
    }

    /// Transcribing while unloaded is a programming error, not a silent empty
    /// string.
    @Test func testTranscribingWhileUnloadedThrows() async {
        let engine = MoonshineSpeechEngine(choice: .moonshineBase, modelDirectory: URL(fileURLWithPath: "/nowhere"))
        do {
            _ = try await engine.transcribe(samples16k: [Float](repeating: 0, count: 16_000))
            Issue.record("transcribe must not run without weights")
        } catch let error as SpeechEngineError {
            guard case .loadFailed = error else {
                Issue.record("expected loadFailed, got \(error)")
                return
            }
        } catch {
            Issue.record("unexpected error \(error)")
        }
    }

    /// Unloading something that was never loaded is a no-op, because
    /// `ModelManager` calls it from a memory warning that has nowhere to put an
    /// error and no idea what state the engine is in.
    @Test func testUnloadingTwiceIsSafe() async {
        let engine = MoonshineSpeechEngine(choice: .moonshineBase, modelDirectory: URL(fileURLWithPath: "/nowhere"))
        await engine.unload()
        await engine.unload()
        #expect(await engine.residentBytes == 0)
    }

    /// The dictionary reaches the engine as terms, in the spelling the user
    /// wants to see, with commas already gone. Moonshine's list is comma
    /// delimited and refuses a term containing one.
    @Test func testTheDictionaryBecomesKeytermsInTheWantedSpelling() async {
        let engine = MoonshineSpeechEngine(choice: .moonshineBase, modelDirectory: URL(fileURLWithPath: "/nowhere"))
        await engine.setKeyterms(fromDictionary: "entro.ly, FastPay, intro dot lie -> entro.ly")
        let terms = await engine.keyterms
        #expect(terms.contains("entro.ly"))
        #expect(terms.contains("FastPay"))
        #expect(terms.allSatisfy { !$0.contains(",") })
        #expect(terms.allSatisfy { !$0.isEmpty })

        await engine.setKeyterms(fromDictionary: "")
        #expect(await engine.keyterms.isEmpty)
    }

    /// The engine and the downloader have to agree on where the weights are, or
    /// the app downloads 141 MB into a directory nothing opens.
    @Test func testTheEngineReadsTheDirectoryTheDownloaderInstallsInto() async {
        let store = ModelStore(directory: FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString))
        for choice in EngineChoice.allCases {
            let engine = MoonshineSpeechEngine(choice: choice, store: store)
            let downloader = ModelDownloader(store: store)
            let pin = ModelDownloader.pin(for: choice)
            #expect(engine.modelDirectory == downloader.directory(for: pin))
        }
    }
}


/// The real-model gate, serialised.
///
/// Swift Testing runs a suite's tests in parallel by default, and these tests
/// measure the process footprint. Four of them loading 141 MB of weights into
/// the same process at once would make every delta a reading of the other three,
/// so `.serialized` is not tidiness here, it is what makes `residentBytes` mean
/// what it says.
///
/// Weights are 141 MB and are not committed, so these read a directory named by
/// `OPENFLOW_MOONSHINE_MODEL_DIR` and skip with a printed reason when it is
/// unset. On this Mac that directory is
/// `~/Library/Caches/moonshine_voice/download.moonshine.ai/model/base-en/quantized/base-en`.
///
/// The skip is deliberately loud. A quietly skipped test reports as a pass, and
/// a recogniser nobody ran is exactly the thing that must not report as a pass.
@Suite(.serialized) struct MoonshineRealModelTests {

    private func modelDirectory(_ function: String = #function) -> URL? {
        MoonshineTestSupport.modelDirectory(function)
    }

    private func fixtureSamples() throws -> [Float] {
        try MoonshineTestSupport.fixtureSamples()
    }


    /// The gate. The reference clip through the real base-en weights, on this
    /// Mac, through the macOS slice of Moonshine's xcframework.
    ///
    /// The assertions are on three content words, case-insensitively:
    /// "leaderboard", "ledger" and "Thursday". Not on the product names, which
    /// is the point of the clip -- the model writes "enterol I" and "fast pay",
    /// and `DictionaryPostPass` is what fixes those. Asserting on the brand
    /// spelling here would be testing a correction this engine does not make.
    @Test func testTheReferenceClipTranscribesThroughTheRealModel() async throws {
        guard let directory = modelDirectory() else { return }
        let samples = try fixtureSamples()
        #expect(samples.count > 16_000 * 8, "the clip is 8.5 s at 16 kHz")

        let engine = MoonshineSpeechEngine(choice: .moonshineBase, modelDirectory: directory)
        let loadStarted = Date()
        try await engine.load()
        let loadSeconds = Date().timeIntervalSince(loadStarted)

        let transcript = try await engine.transcribe(samples16k: samples)

        print("""

            === Moonshine base-en, reference clip ===
            transcript: \(transcript.text)
            latency:    \(String(format: "%.3f", transcript.latencySeconds)) s
            load:       \(String(format: "%.3f", loadSeconds)) s
            resident:   \(await engine.residentBytes) bytes (phys_footprint delta)
            =========================================

            """)

        let lower = transcript.text.lowercased()
        #expect(lower.contains("leaderboard"))
        #expect(lower.contains("ledger"))
        #expect(lower.contains("thursday"))
        #expect(transcript.latencySeconds < 2.0, "warm inference on the reference clip is under 2 s")
        #expect(transcript.latencySeconds > 0)

        await engine.unload()
        #expect(await engine.residentBytes == 0)
    }

    /// `load()` is idempotent: a second call while loaded returns without
    /// constructing a second `Transcriber`.
    ///
    /// `ModelManager` prewarms on capture start and again on the Action Button
    /// intent (PLAN.md section 2), so two loads for one take is the ordinary
    /// case. Without the guard the second one allocates a second copy of the
    /// weights and leaves the first with nothing holding it, which is 141 MB
    /// that neither shows up in the state machine nor comes back on unload.
    /// `loadCount` is the only thing that changes observably, which is why the
    /// engine exposes it.
    @Test func testLoadingTwiceDoesNotConstructASecondTranscriber() async throws {
        guard let directory = modelDirectory() else { return }
        let engine = MoonshineSpeechEngine(choice: .moonshineBase, modelDirectory: directory)

        try await engine.load()
        #expect(await engine.loadCount == 1)
        let firstFootprint = await engine.residentBytes

        try await engine.load()
        #expect(await engine.loadCount == 1, "a second load must not construct a second Transcriber")
        #expect(await engine.residentBytes == firstFootprint, "and must not change what is resident")

        // The engine still works after the second call, so the guard is not
        // returning early out of a half-built state.
        let transcript = try await engine.transcribe(samples16k: try fixtureSamples())
        #expect(transcript.text.lowercased().contains("thursday"))

        // And a reload after an unload does count, so the guard is keyed on
        // being loaded rather than on having ever loaded.
        await engine.unload()
        try await engine.load()
        #expect(await engine.loadCount == 2)
        await engine.unload()
    }

    /// The empty-transcript rule: a take that recognised nothing throws rather
    /// than handing back an empty string that reaches the clipboard.
    ///
    /// Eight seconds of digital silence, through the real model, which is what
    /// the desktop's whole-take silence gate exists to keep out and what this
    /// engine has to survive when the gate is off.
    @Test func testSilenceThrowsRatherThanReturningAnEmptyTranscript() async throws {
        guard let directory = modelDirectory() else { return }
        let engine = MoonshineSpeechEngine(choice: .moonshineBase, modelDirectory: directory)
        try await engine.load()
        defer { Task { await engine.unload() } }

        do {
            let transcript = try await engine.transcribe(
                samples16k: [Float](repeating: 0, count: 16_000 * 8)
            )
            Issue.record("silence must not produce a transcript, got \"\(transcript.text)\"")
        } catch let error as SpeechEngineError {
            #expect(error == .noSpeechRecognised)
        }
    }

    /// The footprint delta is a measurement, so what is asserted is that it is a
    /// measurement: zero when unloaded, at least the weights on disk when
    /// loaded, and back to zero after.
    ///
    /// No upper bound. `M2-MOONSHINE.md` records 562 MB of whole-process RSS on
    /// an M4 for base-en, but that is a different quantity measured in a
    /// different process, and a ceiling copied across from it would fail on
    /// somebody else's machine for no reason anybody could act on.
    @Test func testResidentBytesIsAMeasuredFootprintDelta() async throws {
        guard let directory = modelDirectory() else { return }
        let engine = MoonshineSpeechEngine(choice: .moonshineBase, modelDirectory: directory)
        #expect(await engine.residentBytes == 0)

        try await engine.load()
        let resident = await engine.residentBytes
        var weights = 0
        for name in ["encoder_model.ort", "decoder_model_merged.ort", "tokenizer.bin"] {
            let attributes = try FileManager.default
                .attributesOfItem(atPath: directory.appendingPathComponent(name).path)
            weights += (attributes[.size] as? NSNumber)?.intValue ?? 0
        }
        #expect(resident >= weights, "the delta is floored at the weights on disk")

        await engine.unload()
        #expect(await engine.residentBytes == 0)
    }
}


/// Shared by both suites: finding the weights, and reading the fixture.
enum MoonshineTestSupport {
    static let modelDirectoryVariable = "OPENFLOW_MOONSHINE_MODEL_DIR"

    /// Nil when the variable is unset or points somewhere without the three
    /// files, having said which on the way past.
    static func modelDirectory(_ function: String) -> URL? {
        guard let raw = ProcessInfo.processInfo.environment[modelDirectoryVariable],
              !raw.trimmingCharacters(in: .whitespaces).isEmpty
        else {
            print("""
                SKIPPED \(function): \(modelDirectoryVariable) is unset, so the real \
                model is not available. Set it to a Moonshine base-en directory holding \
                encoder_model.ort, decoder_model_merged.ort and tokenizer.bin, then run \
                `swift test` again.
                """)
            return nil
        }
        let url = URL(fileURLWithPath: (raw as NSString).expandingTildeInPath, isDirectory: true)
        for name in ["encoder_model.ort", "decoder_model_merged.ort", "tokenizer.bin"] {
            guard FileManager.default.fileExists(atPath: url.appendingPathComponent(name).path) else {
                print("SKIPPED \(function): \(url.path) has no \(name).")
                return nil
            }
        }
        return url
    }

    /// The reference clip: 8.5 s, 16 kHz mono Int16, the sentence from
    /// `M2-MOONSHINE.md` spoken by the macOS Samantha voice. Committed, because
    /// 270 KB of fixture is what makes this test reproducible.
    static func fixtureSamples() throws -> [Float] {
        let url = try #require(
            Bundle.module.url(forResource: "Resources/take-say", withExtension: "wav"),
            "the reference clip must be in the test bundle"
        )
        return try WAVFixture.monoFloat16k(at: url)
    }
}

/// Just enough WAV to read the fixture: 16-bit PCM, mono, 16 kHz.
///
/// Written here rather than taken from a framework because the engine's input is
/// `[Float]` from `AudioCapture`, and a test that went through `AVAudioFile`
/// would be testing a path the app does not use on this input.
enum WAVFixture {
    enum FixtureError: Error, CustomStringConvertible {
        case notPCM16Mono16k(String)

        var description: String {
            switch self {
            case let .notPCM16Mono16k(detail): return "fixture is not 16 kHz mono PCM16: \(detail)"
            }
        }
    }

    static func monoFloat16k(at url: URL) throws -> [Float] {
        let data = try Data(contentsOf: url)
        func u32(_ offset: Int) -> UInt32 {
            UInt32(data[offset]) | UInt32(data[offset + 1]) << 8
                | UInt32(data[offset + 2]) << 16 | UInt32(data[offset + 3]) << 24
        }
        func u16(_ offset: Int) -> UInt16 {
            UInt16(data[offset]) | UInt16(data[offset + 1]) << 8
        }

        guard data.count > 44,
              data[0..<4].elementsEqual(Array("RIFF".utf8)),
              data[8..<12].elementsEqual(Array("WAVE".utf8))
        else { throw FixtureError.notPCM16Mono16k("no RIFF/WAVE header") }

        var cursor = 12
        var channels = 0
        var sampleRate = 0
        var bitsPerSample = 0
        var samples: [Float] = []

        while cursor + 8 <= data.count {
            let identifier = String(decoding: data[cursor..<(cursor + 4)], as: UTF8.self)
            let size = Int(u32(cursor + 4))
            let body = cursor + 8
            guard body + size <= data.count else { break }

            if identifier == "fmt " {
                channels = Int(u16(body + 2))
                sampleRate = Int(u32(body + 4))
                bitsPerSample = Int(u16(body + 14))
            } else if identifier == "data" {
                guard channels == 1, sampleRate == 16_000, bitsPerSample == 16 else {
                    throw FixtureError.notPCM16Mono16k(
                        "channels=\(channels) rate=\(sampleRate) bits=\(bitsPerSample)"
                    )
                }
                samples.reserveCapacity(size / 2)
                for offset in stride(from: body, to: body + size - 1, by: 2) {
                    let raw = Int16(bitPattern: u16(offset))
                    samples.append(Float(raw) / 32_768)
                }
            }
            cursor = body + size + (size % 2)
        }

        guard !samples.isEmpty else { throw FixtureError.notPCM16Mono16k("no data chunk") }
        return samples
    }
}
