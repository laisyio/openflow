import Foundation
import Testing
@testable import OpenFlowMobileCore

/// Every trigger in PLAN.md section 2, driven on a hand-cranked clock.
@Suite struct ModelManagerTests {

    private func makeManager(
        loadSeconds: Double = 0,
        transcribeSeconds: Double = 0,
        policy: ModelPolicy = ModelPolicy(),
        conditions: MutableConditions = MutableConditions(),
        clock: ManualClock = ManualClock()
    ) -> (ModelManager, FakeEngine, MutableConditions, ManualClock) {
        let engine = FakeEngine(
            clock: clock,
            loadSeconds: loadSeconds,
            transcribeSeconds: transcribeSeconds
        )
        let manager = ModelManager(engine: engine, conditions: conditions, clock: clock, policy: policy)
        return (manager, engine, conditions, clock)
    }

    // MARK: - Load triggers

    /// The single largest latency win on the phone: the load runs while the user
    /// is still speaking, so by the time they stop the model is already there.
    @Test func testPrewarmOverlapsRecordingAndIsReadyWhenSpeechStops() async throws {
        let (manager, engine, _, clock) = makeManager(loadSeconds: 3)

        let started = await manager.prewarm(trigger: .captureStart)
        #expect(started)
        let duringSpeech = await manager.state
        #expect(duringSpeech == .loading, "the load must run alongside the recording")

        // Three seconds of speech pass; the load lands inside them.
        await clock.advanceOnceSleeping(1, by: 3)
        await waitUntil("model reaches ready") { await manager.state == .ready }

        let transcript = try await manager.transcribe(samples16k: [0.1, 0.2, 0.3])
        #expect(!transcript.text.isEmpty)
        let loads = await engine.loadCount
        #expect(loads == 1, "the take must reuse the prewarmed model, not load a second time")
    }

    @Test func testIntentPrewarmUsesTheSameMachinery() async throws {
        let (manager, _, _, _) = makeManager()
        let started = await manager.prewarm(trigger: .intentPrewarm)
        #expect(started)
        await waitUntil("ready") { await manager.state == .ready }
        let log = await manager.transitions
        #expect(log.first?.trigger == .intentPrewarm)
    }

    /// PLAN.md section 2: never prewarm in Low Power Mode. Load only when there
    /// is audio to transcribe.
    @Test func testNoPrewarmInLowPowerModeButAudioStillLoadsOnDemand() async throws {
        let conditions = MutableConditions()
        conditions.set(lowPowerMode: true)
        let (manager, engine, _, _) = makeManager(conditions: conditions)

        let started = await manager.prewarm(trigger: .captureStart)
        #expect(!started)
        let state = await manager.state
        #expect(state == .unloaded)
        let refusals = await manager.prewarmRefusals
        #expect(refusals.map(\.reason) == [.lowPowerMode])
        let loadsAfterRefusal = await engine.loadCount
        #expect(loadsAfterRefusal == 0, "a refused prewarm must not load anything")

        _ = try await manager.transcribe(samples16k: [0.1])
        let loads = await engine.loadCount
        #expect(loads == 1, "audio waiting must still load the model")
        let after = await manager.state
        #expect(after == .ready)
    }

    @Test func testNoPrewarmAtSeriousThermalPressure() async {
        let conditions = MutableConditions()
        conditions.set(thermal: .serious)
        let (manager, _, _, _) = makeManager(conditions: conditions)

        let started = await manager.prewarm()
        #expect(!started)
        let refusals = await manager.prewarmRefusals
        #expect(refusals.map(\.reason) == [.thermalPressure])

        // `.fair` is not `.serious`; the rule must not creep.
        conditions.set(thermal: .fair)
        let allowed = await manager.prewarm()
        #expect(allowed, "fair thermal state must still prewarm")
    }

    @Test func testPrewarmSettingOffRefusesBothTriggers() async {
        var policy = ModelPolicy()
        policy.prewarmOnCapture = false
        let (manager, _, _, _) = makeManager(policy: policy)

        let capture = await manager.prewarm(trigger: .captureStart)
        let intent = await manager.prewarm(trigger: .intentPrewarm)
        #expect(!capture)
        #expect(!intent)
        let refusals = await manager.prewarmRefusals
        #expect(refusals.map(\.reason) == [.settingDisabled, .settingDisabled])
    }

    // MARK: - Unload triggers

    @Test func testIdleTimerUnloadsAfterTheConfiguredMinutes() async throws {
        var policy = ModelPolicy()
        policy.unloadAfterMinutes = 5
        let (manager, engine, _, clock) = makeManager(policy: policy)

        _ = try await manager.transcribe(samples16k: [0.1])
        let ready = await manager.state
        #expect(ready == .ready)

        // Four minutes is not five.
        await clock.waitForSleepers(1)
        clock.advance(by: 240)
        let stillReady = await manager.state
        #expect(stillReady == .ready, "the idle timer must not fire early")

        clock.advance(by: 60)
        await waitUntil("idle unload") { await manager.state == .unloaded }
        let unloads = await engine.unloadCount
        #expect(unloads == 1)
        let log = await manager.transitions
        #expect(log.last?.trigger == .idleTimeout)
    }

    /// Zero means "keep loaded while the app is open" (PLAN.md section 2).
    @Test func testIdleTimerIsDisabledWhenTheSettingSaysKeepLoaded() async throws {
        var policy = ModelPolicy()
        policy.unloadAfterMinutes = 0
        let (manager, _, _, clock) = makeManager(policy: policy)

        _ = try await manager.transcribe(samples16k: [0.1])
        clock.advance(by: 60 * 60)
        await Task.yield()
        let state = await manager.state
        #expect(state == .ready, "keep-loaded must survive an hour of idling")
        #expect(clock.sleeperCount == 0, "no timer should be armed at all")
    }

    /// The race the generation counter exists for: a timer that woke a moment
    /// before `cancelIdleTimer()` is already on its way to the actor, and lands
    /// after a fresh timer has replaced `idleTask`. An `idleTask != nil` guard
    /// waves it through and unloads the model the new take just loaded.
    ///
    /// The interleaving cannot be scheduled reliably from outside an actor, so
    /// the stale wake-up is delivered by hand -- and the same call with the live
    /// generation is made straight afterwards, so a guard that rejected
    /// everything could not pass this test.
    @Test func testAStaleIdleWakeUpCannotUnloadTheModelItDidNotArm() async throws {
        var policy = ModelPolicy()
        policy.unloadAfterMinutes = 5
        let (manager, engine, _, _) = makeManager(policy: policy)

        _ = try await manager.transcribe(samples16k: [0.1])
        let stale = await manager.currentIdleGeneration

        // A second take cancels the first timer and arms a new one.
        _ = try await manager.transcribe(samples16k: [0.1])
        let live = await manager.currentIdleGeneration
        #expect(stale != live, "re-arming must move the generation on")

        await manager.idleElapsed(generation: stale)
        let survived = await manager.state
        #expect(survived == .ready, "a stale wake-up must not unload the new take's model")
        let unloadsAfterStale = await engine.unloadCount
        #expect(unloadsAfterStale == 0)

        await manager.idleElapsed(generation: live)
        let unloaded = await manager.state
        #expect(unloaded == .unloaded, "the live wake-up must still work, or the guard proves nothing")
    }

    /// A backgrounded app with nothing resident should hold no timer at all.
    @Test func testBackgroundingWithNothingLoadedArmsNoTimer() async {
        var policy = ModelPolicy()
        policy.backgroundGraceSeconds = 20
        let (manager, engine, _, clock) = makeManager(policy: policy)

        let idle = await manager.state
        #expect(idle == .unloaded)
        await manager.handleEnterBackground()
        #expect(clock.sleeperCount == 0, "nothing resident, nothing to schedule")

        clock.advance(by: 120)
        await Task.yield()
        let after = await manager.state
        #expect(after == .unloaded)
        let unloads = await engine.unloadCount
        #expect(unloads == 0)
    }

    @Test func testMemoryWarningUnloadsImmediately() async throws {
        let (manager, engine, _, _) = makeManager()
        try await manager.ensureLoaded()
        let ready = await manager.state
        #expect(ready == .ready)

        await manager.handleMemoryWarning()
        let state = await manager.state
        #expect(state == .unloaded, "a memory warning does not get a grace period")
        let unloads = await engine.unloadCount
        #expect(unloads == 1)
        let resident = await manager.residentBytes
        #expect(resident == 0)
        let log = await manager.transitions
        #expect(log.last?.trigger == .memoryWarning)
    }

    /// PLAN.md section 2, the sentence that matters most: finish the
    /// transcription, deliver the text, *then* unload. Losing the user's words
    /// to save a gigabyte is not a trade the app is allowed to make.
    @Test func testBackgroundUnloadWaitsForAnInFlightTranscription() async throws {
        var policy = ModelPolicy()
        policy.backgroundGraceSeconds = 20
        policy.unloadAfterMinutes = 5
        let (manager, engine, _, clock) = makeManager(transcribeSeconds: 600, policy: policy)

        try await manager.ensureLoaded()
        let work = Task { try await manager.transcribe(samples16k: [0.1, 0.2]) }
        await waitUntil("transcription admitted") { await manager.transcriptionsInFlight == 1 }
        await clock.waitForSleepers(1)   // the engine is mid-transcription
        let inFlight = await manager.transcriptionsInFlight
        #expect(inFlight == 1)

        await manager.handleEnterBackground()
        await clock.waitForSleepers(2)   // plus the background grace timer
        clock.advance(by: 20)

        // The grace period has elapsed and the model is still resident, because
        // there is text that has not been delivered yet.
        await waitUntil("grace elapsed") { await manager.transcriptionsInFlight == 1 }
        let stillReady = await manager.state
        #expect(stillReady == .ready, "an in-flight take must hold the unload off")
        let unloadsSoFar = await engine.unloadCount
        #expect(unloadsSoFar == 0)

        clock.advance(by: 600)
        let transcript = try await work.value
        #expect(!transcript.text.isEmpty, "the text must survive the background unload")

        await waitUntil("deferred unload") { await manager.state == .unloaded }
        let log = await manager.transitions
        #expect(log.last?.trigger == .backgroundDelay)
    }

    @Test func testBackgroundUnloadFiresWhenNothingIsInFlight() async throws {
        var policy = ModelPolicy()
        policy.backgroundGraceSeconds = 20
        policy.unloadAfterMinutes = 0
        let (manager, _, _, clock) = makeManager(policy: policy)
        try await manager.ensureLoaded()

        await manager.handleEnterBackground()
        await clock.waitForSleepers(1)
        clock.advance(by: 19)
        let early = await manager.state
        #expect(early == .ready, "19 seconds is not 20")

        clock.advance(by: 1)
        await waitUntil("background unload") { await manager.state == .unloaded }
    }

    @Test func testReturningToTheForegroundCancelsThePendingUnload() async throws {
        var policy = ModelPolicy()
        policy.backgroundGraceSeconds = 20
        let (manager, engine, _, clock) = makeManager(policy: policy)
        try await manager.ensureLoaded()

        await manager.handleEnterBackground()
        await clock.waitForSleepers(1)
        await manager.handleEnterForeground()
        clock.advance(by: 120)
        await Task.yield()

        let state = await manager.state
        #expect(state == .ready)
        let unloads = await engine.unloadCount
        #expect(unloads == 0)
    }

    @Test func testSeriousThermalStateUnloads() async throws {
        let conditions = MutableConditions()
        let (manager, _, _, _) = makeManager(conditions: conditions)
        try await manager.ensureLoaded()

        conditions.set(thermal: .fair)
        await manager.handleThermalChange()
        let fair = await manager.state
        #expect(fair == .ready, "fair heat is not an unload trigger")

        conditions.set(thermal: .serious)
        await manager.handleThermalChange()
        let serious = await manager.state
        #expect(serious == .unloaded)
        let log = await manager.transitions
        #expect(log.last?.trigger == .thermal)
    }

    // MARK: - Failure

    @Test func testLoadFailureIsVisibleAndRecoverable() async throws {
        let (manager, engine, _, _) = makeManager()
        await engine.setLoadError(.modelUnavailable("weights missing"))

        do {
            _ = try await manager.transcribe(samples16k: [0.1])
            Issue.record("a failed load must surface to the caller")
        } catch {
            // expected
        }
        let failed = await manager.state
        #expect(failed.isFailed)
        #expect(failed.describedForUI == "Failed: Model unavailable: weights missing")

        // A memory warning must not quietly erase the reason from the screen.
        await manager.handleMemoryWarning()
        let stillFailed = await manager.state
        #expect(stillFailed.isFailed)

        await engine.setLoadError(nil)
        let transcript = try await manager.transcribe(samples16k: [0.1])
        #expect(!transcript.text.isEmpty)
        let recovered = await manager.state
        #expect(recovered == .ready, "the download screen's retry has to be able to work")
    }

    @Test func testTranscriptionFailureDoesNotStrandTheInFlightCount() async throws {
        let (manager, engine, _, _) = makeManager()
        await engine.setTranscribeError(.noSpeechRecognised)
        do {
            _ = try await manager.transcribe(samples16k: [0.1])
            Issue.record("expected a throw")
        } catch {}
        let inFlight = await manager.transcriptionsInFlight
        #expect(inFlight == 0, "a failed take must still release the unloaders")
    }

    // MARK: - Diagnostics

    @Test func testTransitionLogRecordsEveryStateWithTimestamps() async throws {
        let clock = ManualClock()
        let (manager, _, _, _) = makeManager(clock: clock)
        try await manager.ensureLoaded()
        clock.advance(by: 42)
        await manager.unloadNow()

        let log = await manager.transitions
        #expect(log.map(\.to) == [.loading, .ready, .unloading, .unloaded])
        #expect(log.map(\.from) == [.unloaded, .loading, .ready, .unloading])
        #expect(log.first?.at == 0)
        #expect(log.last?.at == 42)
        #expect(log.allSatisfy { $0.wallClock.timeIntervalSince1970 > 0 })
    }

    @Test func testResidentBytesReportsTheCostOnlyWhileLoaded() async throws {
        let (manager, _, _, _) = makeManager()
        let idle = await manager.residentBytes
        #expect(idle == 0)
        try await manager.ensureLoaded()
        let loaded = await manager.residentBytes
        #expect(loaded > 0)
        await manager.unloadNow()
        let dropped = await manager.residentBytes
        #expect(dropped == 0)
    }
}
