import Foundation
import Testing
import OpenFlowMobileCore
@testable import OpenFlow

@Suite @MainActor
struct DictationControllerTests {
    private func settings() -> SettingsStore {
        let store = SettingsStore(defaults: UserDefaults(suiteName: "openflow.controller.tests.\(UUID().uuidString)")!)
        store.hapticOnStop = false
        return store
    }

    private func waitFor(_ message: String, _ predicate: @escaping @MainActor () async -> Bool) async {
        for _ in 0..<2_000 {
            if await predicate() { return }
            try? await Task.sleep(for: .milliseconds(1))
        }
        Issue.record("Timed out: \(message)")
    }

    @Test func cancellationDuringPermissionPreventsLateRecording() async {
        let microphone = TestMicrophone(waitForPermission: true)
        let engine = TestSpeechEngine()
        let controller = DictationController(settings: settings(), capture: microphone, repository: nil,
                                            clipboard: NoopClipboardWriter(), enablesSystemSurfaces: false,
                                            engineFactory: { _ in engine })
        let starting = Task { await controller.startRecording() }
        await waitFor("permission") { await microphone.startCount == 1 }
        let cancelling = Task { await controller.cancelRecording() }
        await waitFor("cancelling") { controller.phase == .cancelling }
        await controller.startRecording()
        await microphone.allowPermission()
        await starting.value
        await cancelling.value
        #expect(controller.phase == .idle)
        #expect(await microphone.startCount == 1)
        #expect(!(await microphone.recording))
    }

    @Test func staleInferenceNeverWritesClipboardOrOverlapsNextTake() async {
        let microphone = TestMicrophone()
        let engine = TestSpeechEngine(blockInference: true)
        let clipboard = TestClipboard()
        let controller = DictationController(settings: settings(), capture: microphone, repository: nil,
                                            clipboard: clipboard, enablesSystemSurfaces: false,
                                            engineFactory: { _ in engine })
        await controller.startRecording()
        let stopping = Task { await controller.stopRecording() }
        await waitFor("inference") { await engine.transcribeCount == 1 }
        await controller.startRecording()
        #expect(await microphone.startCount == 1)
        let cancelling = Task { await controller.cancelRecording() }
        await waitFor("cancelling") { controller.phase == .cancelling }
        await engine.finishInference()
        await stopping.value
        await cancelling.value
        #expect(clipboard.texts.isEmpty)
        #expect(controller.phase == .idle)
        #expect(controller.canStart)
    }

    @Test func cancellationDuringAudioStopReleasesAdmission() async {
        let microphone = TestMicrophone(waitForStop: true)
        let engine = TestSpeechEngine()
        let controller = DictationController(settings: settings(), capture: microphone, repository: nil,
                                            clipboard: NoopClipboardWriter(), enablesSystemSurfaces: false,
                                            engineFactory: { _ in engine })
        await controller.startRecording()
        let stopping = Task { await controller.stopRecording() }
        await waitFor("audio stop") { await microphone.stopCount == 1 }
        let cancelling = Task { await controller.cancelRecording() }
        await waitFor("cancelling") { controller.phase == .cancelling }
        #expect(!controller.canStart)
        await microphone.finishStop()
        await stopping.value
        await cancelling.value
        #expect(controller.phase == .idle)
        #expect(controller.canStart)
        #expect(await engine.transcribeCount == 0)
    }

    @Test func engineChoiceSwitchesAfterCaptureAndAppliesToNextTake() async {
        let microphone = TestMicrophone()
        let base = TestSpeechEngine()
        let tiny = TestSpeechEngine()
        let settings = settings()
        let controller = DictationController(settings: settings, capture: microphone, repository: nil,
                                            clipboard: NoopClipboardWriter(), enablesSystemSurfaces: false,
                                            engineFactory: { $0 == .moonshineBase ? base : tiny })
        await controller.startRecording()
        settings.engine = .moonshineTiny
        await controller.applyChangedSettings()
        #expect(controller.activeEngine == .moonshineBase)
        await controller.stopRecording()
        #expect(controller.activeEngine == .moonshineTiny)
        #expect(!(await base.loaded))
        await controller.startRecording()
        await controller.stopRecording()
        #expect(await base.transcribeCount == 1)
        #expect(await tiny.transcribeCount == 1)
        settings.engine = .moonshineBase
        await controller.applyChangedSettings()
        #expect(controller.activeEngine == .moonshineBase)
        #expect(!(await tiny.loaded))
    }

    @Test func missingSelectedModelFailsClearlyAndCanRetryAfterSwitching() async {
        let settings = settings()
        let base = TestSpeechEngine()
        let missingTiny = TestSpeechEngine(missingWeights: true)
        let controller = DictationController(settings: settings, capture: TestMicrophone(), repository: nil,
                                            clipboard: NoopClipboardWriter(), enablesSystemSurfaces: false,
                                            engineFactory: { $0 == .moonshineBase ? base : missingTiny })
        settings.engine = .moonshineTiny
        await controller.applyChangedSettings()
        await controller.startRecording()
        await controller.stopRecording()
        guard case .failed(let message) = controller.phase else {
            Issue.record("A missing selected model must fail visibly")
            return
        }
        #expect(message.contains("Missing test weights"))
        #expect(controller.canRetry)
        settings.engine = .moonshineBase
        await controller.applyChangedSettings()
        await controller.retryTranscription()
        #expect(controller.phase == .finished("Recognised text"))
        #expect(await base.transcribeCount == 1)
        #expect(await missingTiny.transcribeCount == 0)
    }

    @Test func sampleCeilingStopsWithSilenceDetectionDisabled() async {
        let microphone = TestMicrophone()
        let engine = TestSpeechEngine()
        let settings = settings()
        #expect(!settings.stopOnSilence)
        let controller = DictationController(settings: settings, capture: microphone, repository: nil,
                                            clipboard: NoopClipboardWriter(), enablesSystemSurfaces: false,
                                            engineFactory: { _ in engine })
        await controller.startRecording()
        await microphone.reachLimit()
        await waitFor("automatic transcription") {
            if case .finished = controller.phase { return true }
            return false
        }
        #expect(!(await microphone.recording))
        #expect(await engine.transcribeCount == 1)
    }
}

@MainActor private final class TestClipboard: ClipboardWriter {
    var texts: [String] = []
    func write(_ text: String, localOnly: Bool, expiresAfter: TimeInterval?) { texts.append(text) }
}

private actor TestMicrophone: AudioCapturing {
    let waitForPermission: Bool
    let waitForStop: Bool
    var startCount = 0
    var stopCount = 0
    var recording = false
    private var permission: CheckedContinuation<Void, Never>?
    private var stopCompletion: CheckedContinuation<Void, Never>?
    private var onLimit: (@Sendable () -> Void)?
    var currentLevel: Float? { 0.1 }

    init(waitForPermission: Bool = false, waitForStop: Bool = false) {
        self.waitForPermission = waitForPermission
        self.waitForStop = waitForStop
    }
    func start(measuringLevel: Bool, onLimit: @escaping @Sendable () -> Void) async throws {
        startCount += 1
        if waitForPermission { await withCheckedContinuation { permission = $0 } }
        try Task.checkCancellation()
        self.onLimit = onLimit
        recording = true
    }
    func allowPermission() { permission?.resume(); permission = nil }
    func reachLimit() { onLimit?() }
    func stop() async -> CaptureResult {
        stopCount += 1
        if waitForStop { await withCheckedContinuation { stopCompletion = $0 } }
        recording = false
        return CaptureResult(samples16k: [0.1, 0.2], seconds: 1, isSilent: false, hitWatchdog: false)
    }
    func finishStop() { stopCompletion?.resume(); stopCompletion = nil }
    func cancel() { recording = false }
}

private actor TestSpeechEngine: SpeechEngine {
    nonisolated let identifier = "test"
    let blockInference: Bool
    let missingWeights: Bool
    var transcribeCount = 0
    var loaded = false
    private var completion: CheckedContinuation<Void, Never>?
    var residentBytes: Int { loaded ? 100 : 0 }

    init(blockInference: Bool = false, missingWeights: Bool = false) {
        self.blockInference = blockInference
        self.missingWeights = missingWeights
    }
    func load() throws {
        if missingWeights { throw SpeechEngineError.modelUnavailable("Missing test weights") }
        loaded = true
    }
    func unload() { loaded = false }
    func transcribe(samples16k: [Float]) async throws -> Transcript {
        transcribeCount += 1
        if blockInference { await withCheckedContinuation { completion = $0 } }
        return Transcript(text: "Recognised text", latencySeconds: 0)
    }
    func finishInference() { completion?.resume(); completion = nil }
}
