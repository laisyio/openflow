import Foundation

/// Every state the model can be in. PLAN.md section 2:
/// `unloaded -> loading -> ready -> unloading -> unloaded`, plus `failed`.
public enum ModelState: Sendable, Equatable {
    case unloaded
    case loading
    case ready
    case unloading
    case failed(String)

    public var isReady: Bool { self == .ready }

    public var isFailed: Bool {
        if case .failed = self { return true }
        return false
    }

    public var describedForUI: String {
        switch self {
        case .unloaded: return "Not loaded"
        case .loading: return "Loading"
        case .ready: return "Ready"
        case .unloading: return "Unloading"
        case .failed(let reason): return "Failed: \(reason)"
        }
    }
}

/// Why a transition happened. Every entry in the diagnostics log carries one.
public enum ModelTrigger: String, Sendable {
    /// Prewarm because the capture sheet started recording (the big latency win).
    case captureStart
    /// Prewarm from the App Intent, before the sheet is even on screen.
    case intentPrewarm
    /// There is audio waiting; load whether or not prewarm was allowed.
    case transcriptionDemand
    /// The idle timer expired after the last transcription.
    case idleTimeout
    /// `didReceiveMemoryWarning`.
    case memoryWarning
    /// The scene has been in the background past the grace delay.
    case backgroundDelay
    /// Thermal state reached `.serious`.
    case thermal
    /// Someone asked directly (Settings screen, tests).
    case manual
}

/// Why a prewarm did not happen. Kept separately from the transition log because
/// a refused prewarm is not a state change, but it is the thing you want to see
/// on the diagnostics screen when someone asks why the first take was slow.
public struct PrewarmRefusal: Sendable, Equatable {
    public let trigger: ModelTrigger
    public let reason: Reason
    public let at: Double

    public enum Reason: String, Sendable {
        case settingDisabled
        case lowPowerMode
        case thermalPressure
        case alreadyLoadedOrLoading
    }
}

/// One line of the diagnostics log.
public struct ModelTransition: Sendable, Equatable {
    public let from: ModelState
    public let to: ModelState
    public let trigger: ModelTrigger
    /// Monotonic reading from the injected clock. Deterministic under test.
    public let at: Double
    /// Wall clock, for a human reading the diagnostics screen.
    public let wallClock: Date
}

/// The knobs section 2 and section 4 give this machine.
public struct ModelPolicy: Sendable, Equatable {
    /// PLAN.md section 4 `prewarmOnCapture`. When false, the model loads only on
    /// demand and both prewarm triggers are refused.
    public var prewarmOnCapture: Bool
    /// PLAN.md section 4 `unloadAfterMinutes`, 1...30. Zero means
    /// "keep loaded while the app is open" and disables the idle timer.
    public var unloadAfterMinutes: Int
    /// PLAN.md section 2: background unload waits this long, and waits longer
    /// still if a transcription is in flight.
    public var backgroundGraceSeconds: Double

    public init(
        prewarmOnCapture: Bool = SettingsStore.Defaults.prewarmOnCapture,
        unloadAfterMinutes: Int = SettingsStore.Defaults.unloadAfterMinutes,
        backgroundGraceSeconds: Double = 20
    ) {
        self.prewarmOnCapture = prewarmOnCapture
        self.unloadAfterMinutes = unloadAfterMinutes
        self.backgroundGraceSeconds = backgroundGraceSeconds
    }
}

/// The smart load/unload state machine from PLAN.md section 2.
///
/// The contract, in one paragraph: loading takes seconds and the user speaks for
/// longer than that, so we start the load the moment recording starts and let it
/// overlap the speech. We refuse that optimisation when the OS has told us it is
/// short of power or hot. We drop the weights aggressively -- idle timer, memory
/// warning, background, heat -- because a suspended app holding a gigabyte is the
/// first thing jetsam kills, and because memory-mapped weights reload from the
/// page cache. The one thing we never do is lose the user's text: a background
/// unload waits for an in-flight transcription to finish first.
public actor ModelManager {
    private var engine: any SpeechEngine
    private let conditions: SystemConditions
    private let clock: IdleClock
    private var policy: ModelPolicy

    private var currentState: ModelState = .unloaded
    private var log: [ModelTransition] = []
    private var refusals: [PrewarmRefusal] = []

    private var loadTask: Task<Void, Never>?
    /// Identifies the load in flight, so a load that was cancelled underneath us
    /// cannot clear or overwrite the state of the one that replaced it.
    private var loadGeneration = 0
    /// The same trick for the idle timer. `Task.cancel()` does not un-resume a
    /// sleeper that has already woken: a timer that fired microseconds before
    /// `cancelIdleTimer()` is already on its way to the actor, and by the time it
    /// arrives `idleTask` has been replaced by a fresh one, so an `idleTask !=
    /// nil` check waves it through and the new take's model is unloaded early.
    /// The generation is what the stale wake-up fails.
    private var idleGeneration = 0
    private var idleTask: Task<Void, Never>?
    private var backgroundTask: Task<Void, Never>?
    private var backgroundGeneration = 0
    private var captureLeases: Set<UUID> = []
    private var isBackground = false
    private var unloadWaiters: [CheckedContinuation<Void, Never>] = []

    /// Transcriptions that have started and not yet delivered their text.
    private var inFlight = 0
    /// Set when an unload was due but a transcription was still running.
    private var unloadWhenIdle: ModelTrigger?

    public init(
        engine: any SpeechEngine,
        conditions: SystemConditions = ProcessInfoConditions(),
        clock: IdleClock = SystemIdleClock(),
        policy: ModelPolicy = ModelPolicy()
    ) {
        self.engine = engine
        self.conditions = conditions
        self.clock = clock
        self.policy = policy
    }

    // MARK: - Observation

    public var state: ModelState { currentState }
    public var transitions: [ModelTransition] { log }
    public var prewarmRefusals: [PrewarmRefusal] { refusals }
    public var transcriptionsInFlight: Int { inFlight }

    /// Bytes the weights hold right now, for the Settings screen.
    public var residentBytes: Int {
        get async { await engine.residentBytes }
    }

    public func update(policy: ModelPolicy) {
        self.policy = policy
        if captureLeases.isEmpty && inFlight == 0 { startIdleTimer() }
    }

    /// A capture owns prewarmed weights until it ends, even before inference.
    public func beginCapture(id: UUID) {
        captureLeases.insert(id)
        cancelIdleTimer()
    }

    public func endCapture(id: UUID) async {
        guard captureLeases.remove(id) != nil else { return }
        await settleAfterWork()
    }

    /// Called only after the controller has settled/cancelled its current take.
    /// The old engine is fully unloaded before its replacement can allocate.
    public func replaceEngine(_ replacement: any SpeechEngine) async throws {
        guard inFlight == 0, captureLeases.isEmpty else {
            throw SpeechEngineError.loadFailed("Finish the current dictation before changing recognisers.")
        }
        await unloadNow()
        engine = replacement
        transition(to: .unloaded, trigger: .manual)
    }

    // MARK: - Load triggers

    /// Prewarm. Returns true when this call started (or joined) a load.
    ///
    /// Refused, with a logged reason, when `prewarmOnCapture` is off, in Low
    /// Power Mode, or at `.serious` thermal pressure or worse. A refusal is not
    /// an error: the model will still load on demand when there is audio.
    @discardableResult
    public func prewarm(trigger: ModelTrigger = .captureStart) -> Bool {
        guard policy.prewarmOnCapture else {
            refuse(trigger, .settingDisabled)
            return false
        }
        let snapshot = conditions.current()
        if snapshot.lowPowerMode {
            refuse(trigger, .lowPowerMode)
            return false
        }
        if snapshot.thermal >= .serious {
            refuse(trigger, .thermalPressure)
            return false
        }
        cancelIdleTimer()
        switch currentState {
        case .ready, .loading, .unloading:
            refuse(trigger, .alreadyLoadedOrLoading)
            if currentState == .ready { startIdleTimer() }
            return false
        case .unloaded, .failed:
            cancelIdleTimer()
            startLoad(trigger: trigger)
            return true
        }
    }

    /// Recognise a take. Loads on demand if a prewarm never happened or was
    /// refused, then holds the unloaders off until the text has been returned.
    public func transcribe(samples16k: [Float]) async throws -> Transcript {
        cancelIdleTimer()
        inFlight += 1
        do {
            try await ensureLoaded(trigger: .transcriptionDemand)
            try Task.checkCancellation()
            let transcript = try await engine.transcribe(samples16k: samples16k)
            try Task.checkCancellation()
            inFlight -= 1
            await settleAfterWork()
            return transcript
        } catch {
            inFlight -= 1
            await settleAfterWork()
            throw error
        }
    }

    /// Load and wait, without the prewarm veto rules. Used by the download
    /// screen's "verify it runs" step and by `transcribe`.
    public func ensureLoaded(trigger: ModelTrigger = .manual) async throws {
        // Two rounds at most: join a load already running, then, if that one
        // failed or was cancelled underneath us, start one of our own. A third
        // failure is a real failure and the caller hears about it.
        for _ in 0..<2 {
            if currentState == .unloading {
                await withCheckedContinuation { unloadWaiters.append($0) }
            }
            try Task.checkCancellation()
            if currentState == .ready { return }
            if let task = loadTask {
                await task.value
                continue
            }
            startLoad(trigger: trigger)
            if let task = loadTask { await task.value }
            if currentState == .ready { return }
            if case .failed(let reason) = currentState {
                throw SpeechEngineError.loadFailed(reason)
            }
        }
        if currentState == .ready { return }
        if case .failed(let reason) = currentState {
            throw SpeechEngineError.loadFailed(reason)
        }
        throw SpeechEngineError.loadFailed("The model did not reach a ready state")
    }

    private func startLoad(trigger: ModelTrigger) {
        guard loadTask == nil else { return }
        transition(to: .loading, trigger: trigger)
        loadGeneration += 1
        let generation = loadGeneration
        loadTask = Task { [weak self] in
            await self?.runLoad(trigger: trigger, generation: generation)
        }
    }

    /// Runs on the actor, so by the time `loadTask.value` returns to a waiter the
    /// state has already settled to `.ready` or `.failed`. That is what lets
    /// `ensureLoaded` be a simple join instead of a poll.
    private func runLoad(trigger: ModelTrigger, generation: Int) async {
        do {
            try await engine.load()
            guard generation == loadGeneration else { return }
            loadTask = nil
            // A memory warning or a background unload may have overtaken the
            // load. Only claim ready if nobody unloaded underneath us.
            if currentState == .loading {
                transition(to: .ready, trigger: trigger)
                startIdleTimer()
            }
        } catch {
            guard generation == loadGeneration else { return }
            loadTask = nil
            // A load cancelled by an unload is not a failure; the unload has
            // already moved the state on and owns what happens next.
            if currentState == .loading {
                transition(to: .failed(describe(error)), trigger: trigger)
            }
        }
    }

    // MARK: - Unload triggers

    /// `didReceiveMemoryWarning`: drop the weights now. PLAN.md section 2 says
    /// immediately, and it means it -- the alternative to a slow next take is
    /// jetsam killing the process and losing the sheet.
    public func handleMemoryWarning() async {
        cancelIdleTimer()
        cancelBackgroundTimer()
        unloadWhenIdle = nil
        await unload(trigger: .memoryWarning)
    }

    /// The scene left the foreground. Unload after the grace delay, unless a
    /// transcription is in flight, in which case finish it first.
    ///
    /// Arms nothing when there is nothing to drop. A backgrounded app with no
    /// model resident should hold no timer at all -- PLAN.md section 5 budgets
    /// zero idle work -- and an unload scheduled against `.unloaded` could only
    /// ever fire against a model some later prewarm had just loaded.
    public func handleEnterBackground() {
        isBackground = true
        cancelIdleTimer()
        cancelBackgroundTimer()
        switch currentState {
        case .ready, .loading:
            break
        case .unloaded, .unloading, .failed:
            return
        }
        let grace = policy.backgroundGraceSeconds
        backgroundGeneration += 1
        let generation = backgroundGeneration
        backgroundTask = Task { [weak self, clock] in
            await clock.sleep(seconds: grace)
            if Task.isCancelled { return }
            await self?.backgroundGraceElapsed(generation: generation)
        }
    }

    private func backgroundGraceElapsed(generation: Int) async {
        guard generation == backgroundGeneration, backgroundTask != nil else { return }
        if inFlight > 0 || !captureLeases.isEmpty {
            // The text has not reached the store and the clipboard yet. Finish,
            // deliver, then unload -- section 2 is explicit about the order.
            unloadWhenIdle = .backgroundDelay
            return
        }
        await unload(trigger: .backgroundDelay)
    }

    /// Back in the foreground: cancel any pending background unload.
    public func handleEnterForeground() {
        isBackground = false
        cancelBackgroundTimer()
        unloadWhenIdle = nil
        startIdleTimer()
    }

    /// Called when the OS posts a thermal-state change. Unloads at `.serious`.
    public func handleThermalChange() async {
        guard conditions.current().demandsUnload else { return }
        cancelIdleTimer()
        if inFlight > 0 {
            unloadWhenIdle = .thermal
            return
        }
        await unload(trigger: .thermal)
    }

    /// Drop the weights now, from Settings or a test.
    public func unloadNow() async {
        cancelIdleTimer()
        cancelBackgroundTimer()
        await unload(trigger: .manual)
    }

    private func unload(trigger: ModelTrigger) async {
        switch currentState {
        case .unloaded:
            return
        case .unloading:
            await withCheckedContinuation { unloadWaiters.append($0) }
            return
        case .failed:
            // Nothing is resident to drop, and the reason is worth keeping on
            // screen until someone retries.
            return
        case .loading:
            let pendingLoad = loadTask
            pendingLoad?.cancel()
            loadGeneration += 1
            loadTask = nil
            transition(to: .unloading, trigger: trigger)
            // A cooperative load may still finish after cancellation. Join it
            // before unloading, so it cannot resurrect weights after cleanup.
            await pendingLoad?.value
        case .ready:
            break
        }
        transition(to: .unloading, trigger: trigger)
        await engine.unload()
        // A prewarm can arrive while `engine.unload()` is suspended; do not
        // stamp `.unloaded` over a load that has already started.
        guard currentState == .unloading else { return }
        transition(to: .unloaded, trigger: trigger)
        let waiters = unloadWaiters
        unloadWaiters.removeAll()
        for waiter in waiters { waiter.resume() }
    }

    // MARK: - Idle timer

    private func settleAfterWork() async {
        guard inFlight == 0, captureLeases.isEmpty else { return }
        if let pending = unloadWhenIdle {
            unloadWhenIdle = nil
            await unload(trigger: pending)
            return
        }
        if conditions.current().demandsUnload {
            await unload(trigger: .thermal)
            return
        }
        if isBackground {
            await unload(trigger: .backgroundDelay)
            return
        }
        startIdleTimer()
    }

    private func startIdleTimer() {
        cancelIdleTimer()
        // Zero means "keep loaded while the app is open" (PLAN.md section 2).
        guard policy.unloadAfterMinutes > 0, currentState == .ready,
              inFlight == 0, captureLeases.isEmpty else { return }
        let seconds = Double(policy.unloadAfterMinutes) * 60
        idleGeneration += 1
        let generation = idleGeneration
        idleTask = Task { [weak self, clock] in
            await clock.sleep(seconds: seconds)
            if Task.isCancelled { return }
            await self?.idleElapsed(generation: generation)
        }
    }

    /// Internal rather than private so a test can deliver a stale wake-up by
    /// hand; the race it guards against cannot be scheduled reliably from
    /// outside the actor.
    func idleElapsed(generation: Int) async {
        guard generation == idleGeneration, idleTask != nil, inFlight == 0,
              captureLeases.isEmpty else { return }
        await unload(trigger: .idleTimeout)
    }

    /// The generation a wake-up must carry to be acted on.
    var currentIdleGeneration: Int { idleGeneration }

    private func cancelIdleTimer() {
        idleTask?.cancel()
        idleTask = nil
        // Anything already in flight towards `idleElapsed` is now stale.
        idleGeneration += 1
    }

    private func cancelBackgroundTimer() {
        backgroundTask?.cancel()
        backgroundTask = nil
        backgroundGeneration += 1
    }

    // MARK: - Logging

    private func transition(to next: ModelState, trigger: ModelTrigger) {
        guard next != currentState else { return }
        let at = clock.nowSeconds()
        let entry = ModelTransition(
            from: currentState,
            to: next,
            trigger: trigger,
            at: at,
            wallClock: Date()
        )
        currentState = next
        log.append(entry)
        // The diagnostics screen shows a session, not a lifetime.
        if log.count > 200 { log.removeFirst(log.count - 200) }
    }

    private func refuse(_ trigger: ModelTrigger, _ reason: PrewarmRefusal.Reason) {
        let at = clock.nowSeconds()
        refusals.append(PrewarmRefusal(trigger: trigger, reason: reason, at: at))
        if refusals.count > 50 { refusals.removeFirst(refusals.count - 50) }
    }

    private func describe(_ error: Error) -> String {
        if let engineError = error as? SpeechEngineError {
            switch engineError {
            case .modelUnavailable(let detail): return "Model unavailable: \(detail)"
            case .loadFailed(let detail): return detail
            case .noSpeechRecognised: return "No speech recognised"
            case .transcriptionFailed(let detail): return detail
            case .acceleratorUnavailable(let detail): return "No GPU or Neural Engine: \(detail)"
            }
        }
        return String(describing: error)
    }
}
