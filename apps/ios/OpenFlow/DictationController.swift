import Foundation
import Observation
import OpenFlowMobileCore

#if canImport(UIKit)
import UIKit
#endif

#if canImport(ActivityKit)
import ActivityKit
#endif

/// What the capture sheet is doing right now.
enum DictationPhase: Equatable {
    case idle
    case recording
    case transcribing
    case finished(String)
    case failed(String)
}

/// The app's one piece of shared state: it owns the model manager, the
/// microphone and the stores, and every screen and the App Intent talk to it.
///
/// Main-actor isolated because everything it drives is UI. The expensive work
/// lives on `ModelManager` and `AudioCapture`, which are their own actors.
@MainActor
@Observable
final class DictationController {
    static let shared = DictationController()

    private(set) var phase: DictationPhase = .idle
    private(set) var modelState: ModelState = .unloaded
    private(set) var residentBytes: Int = 0
    private(set) var history: [TranscriptRecord] = []
    /// One line the capture sheet shows under a finished transcript when there
    /// is something about the take the user could not otherwise tell, which is
    /// currently only the length ceiling. Cleared when the next take starts.
    private(set) var captureNotice: String?
    /// Set by the App Intent so the app opens straight onto the capture sheet.
    var isCaptureSheetPresented = false

    let settings: SettingsStore
    private let manager: ModelManager
    private let clipboard: any ClipboardWriter
    private let store: TranscriptStore?
    private let engineIdentifier: String

    #if canImport(AVFoundation)
    private let capture = AudioCapture()
    #endif

    private var silenceRunSeconds: Double = 0
    private var levelPollTask: Task<Void, Never>?

    #if canImport(ActivityKit)
    private var activity: Activity<DictationActivityAttributes>?
    #endif

    private init() {
        let settings = SettingsStore.shared()
        self.settings = settings

        // The Simulator has no weights and no Metal-backed engine, so a build
        // with -D OPENFLOW_FAKE_ENGINE exercises the whole product -- sheet,
        // history, keyboard, Live Activity -- against a stub. PLAN.md section 6.
        #if OPENFLOW_FAKE_ENGINE
        let engine: any SpeechEngine = FakeEngine(loadSeconds: 0, transcribeSeconds: 0)
        #else
        let engine: any SpeechEngine = UnavailableEngine(choice: settings.engine)
        #endif
        self.engineIdentifier = engine.identifier
        self.manager = ModelManager(
            engine: engine,
            conditions: ProcessInfoConditions(),
            clock: SystemIdleClock(),
            policy: settings.modelPolicy
        )
        self.clipboard = SystemClipboardWriter()
        self.store = try? TranscriptStore.shared()

        // The App Intent lives in a file the widget extension also compiles, so
        // it reaches the controller through this hook rather than by importing
        // the app.
        DictationIntentBridge.shared.register { [weak self] in
            guard let self else { return }
            await self.prewarm(trigger: .intentPrewarm)
            self.isCaptureSheetPresented = true
            await self.startRecording()
        }
        Task { await self.refresh() }
    }

    // MARK: - Lifecycle wiring

    func applyChangedSettings() async {
        await manager.update(policy: settings.modelPolicy)
    }

    func handleMemoryWarning() async {
        await manager.handleMemoryWarning()
        await refresh()
    }

    /// The three scene phases, kept apart on purpose.
    ///
    /// `.inactive` is not backgrounding: it is the app switcher, Control Centre
    /// pulled down, a call banner, Face ID over the top. Treating it as
    /// backgrounding armed the 20 s unload every time the user glanced at
    /// Control Centre, so the model was gone by the time they came back.
    enum LifecyclePhase {
        case active
        case inactive
        case background
    }

    func handleScenePhase(_ phase: LifecyclePhase) async {
        switch phase {
        case .active:
            await manager.handleEnterForeground()
        case .background:
            await manager.handleEnterBackground()
        case .inactive:
            break
        }
        await refresh()
    }

    func handleThermalChange() async {
        await manager.handleThermalChange()
        await refresh()
    }

    /// The two numbers the sheet and the Settings screen draw from.
    ///
    /// It does not touch history. This runs after every take, every scene
    /// change, every memory warning and every thermal notification, and it used
    /// to read and decode the whole history file each time: a glance at Control
    /// Centre cost a full parse of a month of dictations, on the main actor,
    /// for a list that was very likely not even on screen.
    func refresh() async {
        modelState = await manager.state
        residentBytes = await manager.residentBytes
    }

    /// The History tab's own load, called from its `.task`.
    ///
    /// Reading the file when the list appears is the only time the whole list is
    /// needed. Everything else that changes it -- a delivery, a delete -- knows
    /// what changed and says so, so the file is read once per visit rather than
    /// once per notification.
    func loadHistory() async {
        guard let store else { return }
        history = store.loadHistory().reversed()
    }

    func transitionLog() async -> [ModelTransition] {
        await manager.transitions
    }

    // MARK: - The capture loop

    /// Called by the App Intent before the sheet is even on screen, and again
    /// when recording actually starts. Both are refused politely in Low Power
    /// Mode; the model still loads when there is audio.
    func prewarm(trigger: ModelTrigger) async {
        await manager.prewarm(trigger: trigger)
        await refresh()
    }

    func startRecording() async {
        guard phase != .recording else { return }
        silenceRunSeconds = 0
        captureNotice = nil
        #if canImport(AVFoundation)
        do {
            // Only stop-on-silence reads a running level, and taking one costs
            // a pass over every block on the audio thread, so the tap is told
            // up front whether anybody is going to ask.
            try await capture.start(measuringLevel: settings.stopOnSilence)
        } catch {
            phase = .failed(describe(error))
            await endLiveActivity(preview: nil)
            return
        }
        #endif
        phase = .recording
        startLiveActivity()
        // The load overlaps the speech: this is the whole latency argument.
        await prewarm(trigger: .captureStart)
        startLevelPolling()
    }

    func stopRecording() async {
        guard phase == .recording else { return }
        levelPollTask?.cancel()
        levelPollTask = nil

        #if canImport(UIKit)
        if settings.hapticOnStop {
            UIImpactFeedbackGenerator(style: .medium).impactOccurred()
        }
        #endif

        #if canImport(AVFoundation)
        let result: CaptureResult
        do {
            result = try await capture.stop()
        } catch AudioCaptureError.tooShort {
            phase = .failed("That was too short to transcribe.")
            await endLiveActivity(preview: nil)
            return
        } catch {
            phase = .failed(describe(error))
            await endLiveActivity(preview: nil)
            return
        }
        guard !result.isSilent else {
            phase = .failed(SilenceGate.rejectionMessage(deviceName: "the microphone"))
            await endLiveActivity(preview: nil)
            return
        }
        // The ceiling has been reported by the capture since it was built and
        // read by nobody. A take that lost its opening looks like a take that
        // started late, and the user has no way to tell those apart, so the
        // sheet says it alongside the transcript rather than instead of it.
        captureNotice = result.hitWatchdog ? CaptureRingBuffer.ceilingNotice : nil
        await transcribe(result)
        #else
        phase = .failed("Audio capture is not available on this platform.")
        #endif
    }

    func cancelRecording() async {
        levelPollTask?.cancel()
        levelPollTask = nil
        #if canImport(AVFoundation)
        await capture.cancel()
        #endif
        await endLiveActivity(preview: nil)
        phase = .idle
    }

    // MARK: - Live Activity

    /// Updated only on state changes, never on a timer (PLAN.md section 5). The
    /// elapsed count in the pill is drawn by SwiftUI from a start date, so it
    /// ticks without a single wake-up on our side.
    private func startLiveActivity() {
        #if canImport(ActivityKit)
        guard ActivityAuthorizationInfo().areActivitiesEnabled, activity == nil else { return }
        activity = try? Activity.request(
            attributes: DictationActivityAttributes(startedAt: Date()),
            content: .init(state: .init(stage: .recording), staleDate: nil)
        )
        #endif
    }

    private func updateLiveActivity(stage: DictationActivityAttributes.Stage, seconds: Double) {
        #if canImport(ActivityKit)
        guard let activity else { return }
        Task {
            await activity.update(.init(state: .init(stage: stage, seconds: seconds), staleDate: nil))
        }
        #endif
    }

    private func endLiveActivity(preview: String?) async {
        #if canImport(ActivityKit)
        guard let activity else { return }
        let trimmed = preview.map { String($0.prefix(80)) }
        await activity.end(
            .init(state: .init(stage: .idle, seconds: 0, preview: trimmed), staleDate: nil),
            dismissalPolicy: .after(.now + 4)
        )
        self.activity = nil
        #endif
    }

    #if canImport(AVFoundation)
    private func transcribe(_ result: CaptureResult) async {
        phase = .transcribing
        updateLiveActivity(stage: .transcribing, seconds: result.seconds)
        await refresh()
        do {
            let transcript = try await manager.transcribe(samples16k: result.samples16k)
            let corrected = DictionaryPostPass.apply(transcript.text, dictionary: settings.dictionary)
            deliver(corrected, seconds: result.seconds)
            phase = .finished(corrected)
            await endLiveActivity(preview: corrected)
        } catch {
            phase = .failed(describe(error))
            await endLiveActivity(preview: nil)
        }
        await refresh()
    }

    /// Clipboard first, then the store. The clipboard is what the user is about
    /// to paste; the store is what the keyboard will insert later.
    private func deliver(_ text: String, seconds: Double) {
        let expiry = settings.clipboardExpirySeconds
        clipboard.write(text, localOnly: true, expiresAfter: expiry > 0 ? Double(expiry) : nil)

        let record = TranscriptRecord(
            text: text,
            durationSeconds: seconds,
            engine: engineIdentifier
        )
        guard let store else { return }
        try? store.saveLast(record)
        if settings.saveHistory {
            try? store.append(record, retentionDays: settings.historyRetentionDays)
        }

        // Bring the History tab up to date without re-reading the file it is
        // already showing. `lastEntry()` reads one small file rather than the
        // whole list, and it reads back what was actually written: a delivery
        // that never reached disk does not put a row on the tab. The tab's own
        // `.task` still reloads from the file the next time it appears.
        if settings.saveHistory, let saved = store.lastEntry(), saved.id == record.id {
            history.insert(saved, at: 0)
        }
    }

    /// Stop-on-silence, when the setting is on: the level has to stay under the
    /// gate's line for `silenceHoldMs` before the sheet ends the take itself.
    private func startLevelPolling() {
        guard settings.stopOnSilence else { return }
        let hold = Double(settings.silenceHoldMs) / 1_000
        levelPollTask = Task { [weak self] in
            let step = 0.1
            while !Task.isCancelled {
                try? await Task.sleep(nanoseconds: UInt64(step * 1_000_000_000))
                guard let self else { return }
                // Nil means the take was opened without level measurement, so
                // there is no reading to judge. Stopping on a number nobody took
                // would end the take on the user, which is the worse failure.
                guard let level = await self.capture.currentLevel else { return }
                if level < SilenceGate.silenceLevel {
                    self.silenceRunSeconds += step
                } else {
                    self.silenceRunSeconds = 0
                }
                if self.silenceRunSeconds >= hold {
                    await self.stopRecording()
                    return
                }
                if await self.capture.watchdogTripped {
                    await self.stopRecording()
                    return
                }
            }
        }
    }
    #endif

    // MARK: - History

    func deleteHistory(id: UUID) async {
        try? store?.delete(id: id)
        await loadHistory()
    }

    func deleteAllHistory() async {
        try? store?.deleteAll()
        history = []
    }

    func copyToClipboard(_ text: String) {
        let expiry = settings.clipboardExpirySeconds
        clipboard.write(text, localOnly: true, expiresAfter: expiry > 0 ? Double(expiry) : nil)
    }

    private func describe(_ error: Error) -> String {
        if let captureError = error as? AudioCaptureError {
            switch captureError {
            case .permissionDenied:
                return "OpenFlow needs the microphone. Turn it on in Settings, Privacy and Security, Microphone."
            case .engineUnavailable(let detail):
                return "The microphone could not start: \(detail)"
            case .notRecording:
                return "There was no recording to stop."
            case .tooShort:
                return "That was too short to transcribe."
            }
        }
        if let engineError = error as? SpeechEngineError {
            switch engineError {
            case .modelUnavailable(let detail): return detail
            case .loadFailed(let detail): return detail
            case .noSpeechRecognised: return "Nothing recognisable was said."
            case .transcriptionFailed(let detail): return detail
            case .acceleratorUnavailable(let detail): return detail
            }
        }
        return (error as NSError).localizedDescription
    }
}

/// The engine slot before M2 fills it. It refuses rather than pretending, which
/// is the same rule PLAN.md section 7 applies to CPU fallback: fail loudly.
actor UnavailableEngine: SpeechEngine {
    nonisolated let identifier: String
    private let choice: EngineChoice

    init(choice: EngineChoice) {
        self.choice = choice
        self.identifier = choice.rawValue
    }

    var residentBytes: Int { 0 }

    func load() async throws {
        throw SpeechEngineError.modelUnavailable(
            "\(choice.displayName) is not built into this version yet. Milestone M2 adds it."
        )
    }

    func unload() async {}

    func transcribe(samples16k: [Float]) async throws -> Transcript {
        throw SpeechEngineError.modelUnavailable("No speech engine is installed.")
    }
}
