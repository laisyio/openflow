import Foundation
import Observation
import OpenFlowMobileCore
#if !OPENFLOW_FAKE_ENGINE
import OpenFlowMoonshineEngine
#endif

#if canImport(UIKit)
import UIKit
#endif

#if canImport(ActivityKit)
import ActivityKit
#endif

/// The app's one piece of shared state: it owns the model manager, the
/// microphone and the stores, and every screen and the App Intent talk to it.
///
/// Main-actor isolated because everything it drives is UI. The expensive work
/// lives on `ModelManager` and `AudioCapture`, which are their own actors.
@MainActor
@Observable
final class DictationController {
    static let shared = DictationController()

    private var session = DictationSession()
    var phase: DictationPhase { session.phase }
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
    private let repository: TranscriptRepository?
    private var engineIdentifier: String
    private(set) var activeEngine: EngineChoice
    private(set) var isSwitchingEngine = false
    private var isSettling = false
    var canStart: Bool { !phase.isBusy && !isSwitchingEngine && !isSettling }
    private var appliedRetentionDays: Int
    private var operation: Task<Void, Never>?
    private var deliveryAuthorization: TranscriptDeliveryAuthorization?
    private let engineFactory: @MainActor @Sendable (EngineChoice) -> any SpeechEngine
    private let enablesSystemSurfaces: Bool
    private var pendingTake: CaptureResult?
    var canRetry: Bool { pendingTake != nil && canStart }

    #if canImport(UIKit)
    private var backgroundTask: UIBackgroundTaskIdentifier = .invalid
    #endif

    #if canImport(AVFoundation)
    private let capture: any AudioCapturing
    #endif

    private var silenceRunSeconds: Double = 0
    private var levelPollTask: Task<Void, Never>?

    #if canImport(ActivityKit)
    private var activity: Activity<DictationActivityAttributes>?
    #endif

    init(settings: SettingsStore = .shared(),
         capture: any AudioCapturing = AudioCapture(),
         repository: TranscriptRepository? = (try? TranscriptStore.shared()).map { TranscriptRepository(store: $0) },
         clipboard: any ClipboardWriter = SystemClipboardWriter(),
         enablesSystemSurfaces: Bool = true,
         engineFactory: @escaping @MainActor @Sendable (EngineChoice) -> any SpeechEngine = DictationController.makeEngine) {
        self.settings = settings
        self.capture = capture
        self.engineFactory = engineFactory
        self.enablesSystemSurfaces = enablesSystemSurfaces

        // A build with -D OPENFLOW_FAKE_ENGINE exercises the whole product --
        // sheet, history, keyboard, Live Activity -- before any weights have been
        // downloaded. PLAN.md section 6.
        let engine = engineFactory(settings.engine)
        self.activeEngine = settings.engine
        self.appliedRetentionDays = settings.historyRetentionDays
        self.engineIdentifier = engine.identifier
        self.manager = ModelManager(
            engine: engine,
            conditions: ProcessInfoConditions(),
            clock: SystemIdleClock(),
            policy: settings.modelPolicy
        )
        self.clipboard = clipboard
        self.repository = repository

        // The App Intent lives in a file the widget extension also compiles, so
        // it reaches the controller through this hook rather than by importing
        // the app.
        if enablesSystemSurfaces { DictationIntentBridge.shared.register { [weak self] in
            guard let self else { return }
            await self.prewarm(trigger: .intentPrewarm)
            self.isCaptureSheetPresented = true
            await self.startRecording()
        } }
        Task {
            await self.refresh()
            await self.loadHistory()
        }
    }

    static func makeEngine(_ choice: EngineChoice) -> any SpeechEngine {
        #if OPENFLOW_FAKE_ENGINE
        return FakeEngine(loadSeconds: 0, transcribeSeconds: 0)
        #else
        return MoonshineSpeechEngine(choice: choice, store: ModelStore.applicationSupportOrTemporary())
        #endif
    }

    // MARK: - Lifecycle wiring

    func applyChangedSettings() async {
        await manager.update(policy: settings.modelPolicy)
        if appliedRetentionDays != settings.historyRetentionDays {
            appliedRetentionDays = settings.historyRetentionDays
            await loadHistory()
        }
        guard canStart, activeEngine != settings.engine else { return }
        isSwitchingEngine = true
        let choice = settings.engine
        let replacement = engineFactory(choice)
        do {
            try await manager.replaceEngine(replacement)
            activeEngine = choice
            engineIdentifier = replacement.identifier
        } catch {
            captureNotice = describe(error)
            settings.engine = activeEngine
        }
        isSwitchingEngine = false
        await refresh()
        // A second picker change may have arrived while unloading the first.
        if activeEngine != settings.engine { await applyChangedSettings() }
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
            await loadHistory()
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
        guard let repository else { return }
        do { history = try await repository.history(retentionDays: settings.historyRetentionDays) }
        catch { captureNotice = "History could not be read: \(describe(error))" }
    }

    func transitionLog() async -> [ModelTransition] {
        await manager.transitions
    }

    // MARK: - The capture loop

    /// Called by the App Intent before the sheet is even on screen, and again
    /// when recording actually starts. Both are refused politely in Low Power
    /// Mode; the model still loads when there is audio.
    func prewarm(trigger: ModelTrigger) async {
        guard !isSwitchingEngine else { return }
        await applyChangedSettings()
        await manager.prewarm(trigger: trigger)
        await refresh()
    }

    func startRecording() async {
        guard canStart, let id = session.begin() else { return }
        deliveryAuthorization = TranscriptDeliveryAuthorization()
        pendingTake = nil
        silenceRunSeconds = 0
        captureNotice = nil
        operation = Task { [weak self] in
            guard let self else { return }
            await self.beginTake(id)
        }
        await operation?.value
    }

    private func beginTake(_ id: UUID) async {
        await manager.beginCapture(id: id)
        guard session.owns(id), !Task.isCancelled else { return }
        #if canImport(AVFoundation)
        do {
            try await capture.start(measuringLevel: settings.stopOnSilence) { [weak self] in
                // A single event at the sample ceiling, independent of the
                // optional silence poll. The audio callback never waits on UI.
                Task { @MainActor [weak self] in
                    guard let self, self.session.owns(id) else { return }
                    await self.stopRecording()
                }
            }
            guard session.owns(id), !Task.isCancelled else {
                await capture.cancel()
                return
            }
            session.advance(id, to: .recording)
            startLiveActivity()
            await manager.prewarm(trigger: .captureStart)
            if session.owns(id), phase == .recording { startLevelPolling(id: id) }
            await refresh()
        } catch {
            guard session.owns(id) else { return }
            isSettling = true
            session.advance(id, to: .failed(describe(error)))
            await manager.endCapture(id: id)
            await endLiveActivity(preview: nil)
            isSettling = false
            await applyChangedSettings()
        }
        #endif
    }

    func stopRecording() async {
        guard phase == .recording, let id = session.id,
              session.advance(id, to: .stopping) else { return }
        levelPollTask?.cancel()
        levelPollTask = nil
        isSettling = true
        beginBackgroundProcessing()
        #if canImport(UIKit)
        if settings.hapticOnStop { UIImpactFeedbackGenerator(style: .medium).impactOccurred() }
        #endif
        operation = Task { [weak self] in
            guard let self else { return }
            await self.finishTake(id)
        }
        await operation?.value
    }

    private func finishTake(_ id: UUID) async {
        #if canImport(AVFoundation)
        do {
            let result = try await capture.stop()
            if session.owns(id), !Task.isCancelled {
                guard !result.isSilent else {
                    throw SpeechEngineError.noSpeechRecognised
                }
                pendingTake = result
                captureNotice = result.hitWatchdog ? CaptureRingBuffer.ceilingNotice : nil
                await transcribe(result, id: id)
            }
        } catch {
            if session.owns(id) {
                session.advance(id, to: .failed(describe(error)))
                await endLiveActivity(preview: nil)
            }
        }
        await manager.endCapture(id: id)
        endBackgroundProcessing()
        await refresh()
        isSettling = false
        await applyChangedSettings()
        #endif
    }

    func cancelRecording(retainingTake: Bool = false, reason: String? = nil) async {
        guard session.id != nil else { return }
        // This atomic admission race is the persistence boundary: cancelling
        // before the repository starts must prevent writes, not only clipboard.
        deliveryAuthorization?.cancel()
        guard let id = session.cancel() else { return }
        levelPollTask?.cancel()
        levelPollTask = nil
        let pending = operation
        pending?.cancel()
        #if canImport(AVFoundation)
        await capture.cancel()
        #endif
        // Moonshine's synchronous decoder cannot be interrupted inside C++.
        // Keep admission closed until it returns; identity blocks its delivery.
        await pending?.value
        await manager.endCapture(id: id)
        if !retainingTake { pendingTake = nil }
        await endLiveActivity(preview: nil)
        endBackgroundProcessing()
        isSettling = false
        session.cancellationFinished(error: reason)
        await applyChangedSettings()
        await refresh()
    }

    func retryTranscription() async {
        guard canRetry, let result = pendingTake, let id = session.beginTranscription() else { return }
        deliveryAuthorization = TranscriptDeliveryAuthorization()
        isSettling = true
        beginBackgroundProcessing()
        operation = Task { [weak self] in
            guard let self else { return }
            await self.manager.beginCapture(id: id)
            await self.transcribe(result, id: id)
            await self.manager.endCapture(id: id)
            self.endBackgroundProcessing()
            await self.refresh()
            self.isSettling = false
            await self.applyChangedSettings()
        }
        await operation?.value
    }

    private func beginBackgroundProcessing() {
        #if canImport(UIKit)
        guard enablesSystemSurfaces else { return }
        guard backgroundTask == .invalid else { return }
        backgroundTask = UIApplication.shared.beginBackgroundTask(withName: "Finish dictation") { [weak self] in
            Task { @MainActor [weak self] in
                guard let self else { return }
                // End the OS lease promptly; cancellation drains the decoder.
                self.endBackgroundProcessing()
                await self.cancelRecording(
                    retainingTake: true,
                    reason: "iOS paused this dictation before it finished. Keep OpenFlow open and tap Retry."
                )
            }
        }
        #endif
    }

    private func endBackgroundProcessing() {
        #if canImport(UIKit)
        guard backgroundTask != .invalid else { return }
        UIApplication.shared.endBackgroundTask(backgroundTask)
        backgroundTask = .invalid
        #endif
    }

    // MARK: - Live Activity

    /// Updated only on state changes, never on a timer (PLAN.md section 5). The
    /// elapsed count in the pill is drawn by SwiftUI from a start date, so it
    /// ticks without a single wake-up on our side.
    private func startLiveActivity() {
        #if canImport(ActivityKit)
        guard enablesSystemSurfaces else { return }
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
    private func transcribe(_ result: CaptureResult, id: UUID) async {
        guard session.owns(id), !Task.isCancelled, let authorization = deliveryAuthorization else { return }
        if phase == .stopping { session.advance(id, to: .transcribing) }
        updateLiveActivity(stage: .transcribing, seconds: result.seconds)
        do {
            let transcript = try await manager.transcribe(samples16k: result.samples16k)
            guard session.owns(id), !Task.isCancelled else { return }
            let corrected = DictionaryPostPass.apply(transcript.text, dictionary: settings.dictionary)
            let record = TranscriptRecord(text: corrected, durationSeconds: result.seconds, engine: engineIdentifier)
            if let repository {
                do {
                    let saved = try await repository.deliver(record, saveHistory: settings.saveHistory,
                                                            retentionDays: settings.historyRetentionDays,
                                                            authorization: authorization)
                    guard session.owns(id), !Task.isCancelled else { return }
                    history = saved
                } catch {
                    guard session.owns(id), !Task.isCancelled else { return }
                    captureNotice = "The text is ready, but history could not be saved: \(describe(error))"
                }
            }
            guard session.owns(id), !Task.isCancelled else { return }
            copyToClipboard(corrected)
            pendingTake = nil
            session.advance(id, to: .finished(corrected))
            await endLiveActivity(preview: corrected)
        } catch {
            guard session.owns(id) else { return }
            session.advance(id, to: .failed(describe(error)))
            await endLiveActivity(preview: nil)
        }
    }

    /// Silence detection only. The sample ceiling is independently emitted by
    /// AudioCapture even when this optional task is never started.
    private func startLevelPolling(id: UUID) {
        guard settings.stopOnSilence else { return }
        let hold = Double(settings.silenceHoldMs) / 1_000
        levelPollTask = Task { [weak self] in
            let step = 0.1
            while !Task.isCancelled {
                do { try await Task.sleep(nanoseconds: UInt64(step * 1_000_000_000)) }
                catch { return }
                guard let self, self.session.owns(id), self.phase == .recording else { return }
                guard let level = await self.capture.currentLevel else { return }
                guard self.session.owns(id), !Task.isCancelled else { return }
                if level < SilenceGate.silenceLevel { self.silenceRunSeconds += step }
                else { self.silenceRunSeconds = 0 }
                if self.silenceRunSeconds >= hold {
                    // Do not await a stop from the task stopRecording cancels.
                    Task { @MainActor [weak self] in
                        guard let self, self.session.owns(id) else { return }
                        await self.stopRecording()
                    }
                    return
                }
            }
        }
    }
    #endif

    // MARK: - History

    func deleteHistory(id: UUID) async {
        guard let repository else { return }
        do { history = try await repository.delete(id: id, retentionDays: settings.historyRetentionDays) }
        catch { captureNotice = "History could not be deleted: \(describe(error))" }
    }

    func deleteAllHistory() async {
        do {
            try await repository?.deleteAll()
            history = []
        } catch { captureNotice = "History could not be deleted: \(describe(error))" }
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
