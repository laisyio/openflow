import Foundation
import Testing
@testable import OpenFlowMobileCore

@Suite struct PerformanceRegressionTests {
    private final class Counter: @unchecked Sendable {
        private let lock = NSLock()
        private var count = 0
        func increment() { lock.withLock { count += 1 } }
        var value: Int { lock.withLock { count } }
    }

    @Test func hardCeilingFiresWithoutSilenceMeasurementAndKeepsOpening() {
        let counter = Counter()
        let buffer = CaptureBuffer(inputRate: 16_000, capacity: 5, onLimit: counter.increment)
        buffer.write(mono: [1, 2, 3, 4])
        #expect(counter.value == 0)
        buffer.write(mono: [5])
        #expect(counter.value == 1, "exactly reaching capacity requests a stop")
        buffer.write(mono: [6, 7, 8])
        #expect(counter.value == 1)
        #expect(buffer.level == nil, "the optional silence meter was never enabled")
        let result = buffer.finish()
        #expect(result.samples == [1, 2, 3, 4, 5])
        #expect(result.overflowed)
    }

    @Test func conversionMatchesBatchAcrossUpsamplingAndBlockBoundaries() {
        for rate in [8_000.0, 11_025.0, 44_100.0, 48_000.0] {
            let audio = tone(440, rate, 0.05)
            let expected = AudioResampler.downsample(audio, from: rate, to: 16_000)
            for blockSize in [1, 7, 4096] {
                var stream = StreamingDownsampler(from: rate, to: 16_000)
                var actual: [Float] = []
                for start in stride(from: 0, to: audio.count, by: blockSize) {
                    actual += stream.process(Array(audio[start..<min(start + blockSize, audio.count)]))
                }
                actual += stream.flush()
                #expect(actual.count == expected.count, "rate \(rate), blocks \(blockSize)")
                #expect(zip(actual, expected).allSatisfy { abs($0 - $1) < 1e-5 })
                #expect(stream.flush().isEmpty)
            }
        }
    }

    @Test func oneLevelProducesTheSameGain() {
        let audio = tone(400, 16_000, 0.1).map { $0 * 0.02 }
        #expect(SilenceGate.autoGain(audio, level: SilenceGate.speechLevel(audio)) == SilenceGate.autoGain(audio))
    }

    @Test func sessionRejectsOverlapAndStaleCompletionUntilCancellationDrains() throws {
        var session = DictationSession()
        let begun = session.begin()
        let first = try #require(begun)
        #expect(session.begin() == nil, "permission/start is already busy")
        #expect(session.advance(first, to: .recording) == true)
        #expect(session.advance(first, to: .stopping) == true)
        #expect(session.advance(first, to: .transcribing) == true)
        #expect(session.begin() == nil, "Record cannot run during inference")
        #expect(session.cancel() == first)
        #expect(session.advance(first, to: .finished("stale")) == false)
        #expect(session.begin() == nil, "the decoder still owns resources")
        session.cancellationFinished()
        let restarted = session.begin()
        let next = try #require(restarted)
        #expect(next != first)
        #expect(session.advance(first, to: .failed("stale")) == false)
        #expect(session.phase == .starting)
    }

    @Test func abandonedPrewarmArmsIdleCleanup() async throws {
        let clock = ManualClock()
        let manager = ModelManager(engine: FakeEngine(), conditions: StaticConditions(), clock: clock)
        await manager.prewarm()
        await waitUntil("ready") { await manager.state == .ready }
        await clock.waitForSleepers(1)
        clock.advance(by: 301)
        await waitUntil("abandoned prewarm released") { await manager.state == .unloaded }
    }

    @Test func captureLeaseDefeatsOldIdleTimerAndSettlesAfterSilentCancel() async throws {
        let clock = ManualClock()
        let manager = ModelManager(engine: FakeEngine(), conditions: StaticConditions(), clock: clock)
        _ = try await manager.transcribe(samples16k: [0.1])
        let stale = await manager.currentIdleGeneration
        let capture = UUID()
        await manager.beginCapture(id: capture)
        await manager.prewarm()
        await manager.idleElapsed(generation: stale)
        #expect(await manager.state == .ready)
        await manager.endCapture(id: capture)
        let live = await manager.currentIdleGeneration
        await manager.idleElapsed(generation: live)
        #expect(await manager.state == .unloaded)
    }

    @Test func engineSwitchUnloadsOldAndRefusesDuringCapture() async throws {
        let old = FakeEngine()
        let replacement = FakeEngine()
        let manager = ModelManager(engine: old, conditions: StaticConditions())
        try await manager.ensureLoaded()
        let lease = UUID()
        await manager.beginCapture(id: lease)
        await #expect(throws: SpeechEngineError.self) { try await manager.replaceEngine(replacement) }
        #expect(await old.isLoaded)
        await manager.endCapture(id: lease)
        try await manager.replaceEngine(replacement)
        #expect(!(await old.isLoaded))
        #expect(await manager.state == .unloaded)
        _ = try await manager.transcribe(samples16k: [0.1])
        #expect(await replacement.transcribeCount == 1)
        #expect(await old.transcribeCount == 0)
    }

    @Test func cancelledLoadCannotResurrectWeightsAfterMemoryWarning() async {
        let clock = ManualClock()
        let engine = FakeEngine(clock: clock, loadSeconds: 10)
        let manager = ModelManager(engine: engine, conditions: StaticConditions(), clock: clock)
        await manager.prewarm()
        await clock.waitForSleepers(1)
        await manager.handleMemoryWarning()
        #expect(await manager.state == .unloaded)
        #expect(await manager.residentBytes == 0)
    }

    @Test func historyReadAppliesRetentionWithoutNewDictation() async throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: directory) }
        let store = TranscriptStore(directory: directory)
        let now = Date()
        let old = TranscriptRecord(text: "expired", createdAt: now.addingTimeInterval(-3 * 86_400))
        let recent = TranscriptRecord(text: "recent", createdAt: now)
        try store.append(old, retentionDays: 30, now: now)
        try store.append(recent, retentionDays: 30, now: now)
        let repository = TranscriptRepository(store: store)
        #expect(try await repository.history(retentionDays: 1, now: now).map(\.text) == ["recent"])
        #expect(store.loadHistory().map(\.text) == ["recent"])
    }

    @Test func directoryInstallFailureRestoresWorkingModel() throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: directory) }
        let store = ModelStore(directory: directory.appendingPathComponent("model"))
        try store.prepare()
        try Data("working".utf8).write(to: store.url(for: "weights"))
        #expect(throws: (any Error).self) {
            try store.replaceDirectory(from: directory.appendingPathComponent("missing-stage"))
        }
        #expect(try String(contentsOf: store.url(for: "weights"), encoding: .utf8) == "working")
    }
}
